use crate::ProducerKeys;
use crate::ice::{self, IceConfig, Transport};
use crate::signaling;
use crate::turn::{self, TurnRelay};
use blit_remote::{
    C2S_CLIPBOARD, C2S_CLOSE, C2S_CREATE, C2S_CREATE_AT, C2S_CREATE_N, C2S_CREATE2, C2S_INPUT,
    C2S_KILL, C2S_MOUSE, C2S_RESTART, C2S_SURFACE_CLOSE, C2S_SURFACE_INPUT, C2S_SURFACE_POINTER,
    C2S_SURFACE_POINTER_AXIS,
};
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::Receive;
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

#[cfg(unix)]
type IpcStream = tokio::net::UnixStream;
#[cfg(windows)]
type IpcStream = tokio::net::windows::named_pipe::NamedPipeClient;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Returns `true` for C2S message tags that mutate state (input, create, kill,
/// etc.).  Read-only consumers have these messages silently dropped.
fn is_write_message(tag: u8) -> bool {
    matches!(
        tag,
        C2S_INPUT
            | C2S_MOUSE
            | C2S_CLIPBOARD
            | C2S_KILL
            | C2S_RESTART
            | C2S_CLOSE
            | C2S_CREATE
            | C2S_CREATE_AT
            | C2S_CREATE_N
            | C2S_CREATE2
            | C2S_SURFACE_INPUT
            | C2S_SURFACE_POINTER
            | C2S_SURFACE_POINTER_AXIS
            | C2S_SURFACE_CLOSE
    )
}

pub async fn handle_peer(
    peer_session_id: String,
    sock_path: String,
    mut signal_rx: mpsc::UnboundedReceiver<serde_json::Value>,
    signal_tx: mpsc::UnboundedSender<String>,
    keys: ProducerKeys,
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
                Err(e) => verbose!("STUN binding failed: {e}"),
            }
        }

        for ts in &turn_servers {
            let result = match ts.transport {
                Transport::Udp => {
                    TurnRelay::allocate_udp(ts.addr, &ts.username, &ts.credential).await
                }
                Transport::Tcp => {
                    TurnRelay::allocate_tcp(
                        ts.addr,
                        ts.tls,
                        &ts.hostname,
                        &ts.username,
                        &ts.credential,
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
                Err(e) => verbose!("TURN allocate ({:?}) failed: {e}", ts.transport),
            }
        }
    }

    // Wait for the SDP offer.  Decrypt via ProducerKeys::open_sealed which
    // tries both the RW and RO consumer keys — the one that works tells us
    // the consumer's access level.
    let (offer, consumer_access): (SdpOffer, crate::Access) = loop {
        match signal_rx.recv().await {
            Some(raw) => {
                let (data, access) = match keys.open_sealed(&raw) {
                    Some(pair) => pair,
                    None => {
                        // Legacy unencrypted peer — treat as plaintext RW.
                        (raw, crate::Access::ReadWrite)
                    }
                };
                if let Some(sdp) = data.get("sdp") {
                    let offer: SdpOffer = serde_json::from_value(sdp.clone())?;
                    break (offer, access);
                }
            }
            None => return Ok(()),
        }
    };

    verbose!("consumer access: {:?}", consumer_access,);

    let answer = rtc.sdp_api().accept_offer(offer)?;
    let answer_json = serde_json::to_value(&answer)?;
    let signal_data = serde_json::json!({ "sdp": answer_json });
    // Seal the answer with the correct consumer key for this peer's access.
    let bk = keys.box_keys_for(consumer_access);
    let msg = signaling::build_sealed_message(&keys.signing, &peer_session_id, &signal_data, &bk);
    signal_tx
        .send(msg)
        .map_err(|e| format!("send failed: {e}"))?;

    let mut blit_conn: Option<IpcStream> = None;
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
                            verbose!("data channel opened: {label}");
                            if label == "blit" {
                                blit_channel = Some(cid);
                                #[cfg(unix)]
                                {
                                    blit_conn =
                                        Some(tokio::net::UnixStream::connect(&sock_path).await?);
                                }
                                #[cfg(windows)]
                                {
                                    blit_conn = Some(
                                        tokio::net::windows::named_pipe::ClientOptions::new()
                                            .open(&sock_path)
                                            .map_err(
                                                |e| -> Box<dyn std::error::Error + Send + Sync> {
                                                    e.to_string().into()
                                                },
                                            )?,
                                    );
                                }
                                established.store(true, Ordering::Relaxed);
                            }
                        }
                        Event::ChannelData(cd) => {
                            if Some(cd.id) == blit_channel
                                && let Some(conn) = &mut blit_conn
                            {
                                let data = &cd.data;
                                if data.len() < 4 {
                                    continue;
                                }
                                let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]])
                                    as usize;
                                if data.len() < 4 + len {
                                    continue;
                                }
                                let payload = &data[4..4 + len];

                                // Enforce read-only: drop mutating messages.
                                if consumer_access == crate::Access::ReadOnly
                                    && !payload.is_empty()
                                    && is_write_message(payload[0])
                                {
                                    continue;
                                }
                                let frame_len = (payload.len() as u32).to_le_bytes();
                                if conn.write_all(&frame_len).await.is_err() {
                                    verbose!("blit-server socket write failed");
                                    return Ok(());
                                }
                                if !payload.is_empty() && conn.write_all(payload).await.is_err() {
                                    verbose!("blit-server socket write failed");
                                    return Ok(());
                                }
                            }
                        }
                        Event::ChannelClose(cid) => {
                            if Some(cid) == blit_channel {
                                verbose!("blit data channel closed");
                                return Ok(());
                            }
                        }
                        Event::IceConnectionStateChange(state) => {
                            verbose!("ICE state: {state:?}");
                            if matches!(state, str0m::IceConnectionState::Disconnected) {
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
                if let Some((peer_addr, data)) = turn_data
                    && let Some(ra) = relay_addr
                        && let Ok(receive) = Receive::new(
                            str0m::net::Protocol::Udp,
                            peer_addr,
                            ra,
                            &data,
                        ) {
                            rtc.handle_input(Input::Receive(Instant::now(), receive))?;
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
                        verbose!("blit-server connection closed");
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
                    Some(raw) => {
                        // Decrypt with the correct consumer key.
                        let data = keys.open_sealed(&raw)
                            .map(|(v, _)| v)
                            .unwrap_or(raw);
                        if let Some(candidate) = data.get("candidate")
                            && let Ok(c) = serde_json::from_value::<Candidate>(candidate.clone()) {
                                rtc.add_remote_candidate(c);
                            }
                    }
                    None => {
                        signaling_alive = false;
                        if !established.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        verbose!("signaling channel closed, WebRTC connection continues");
                    }
                }
            }
        }
    }
}
