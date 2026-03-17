use crate::ProducerKeys;
use crate::ice::{self, IceConfig, Transport};
use crate::signaling;
use crate::turn::{self, TurnRelay};
use blit_remote::{
    C2S_CLIPBOARD, C2S_CLOSE, C2S_CREATE, C2S_CREATE_AT, C2S_CREATE_N, C2S_CREATE2, C2S_INPUT,
    C2S_KILL, C2S_MOUSE, C2S_RESTART, C2S_SURFACE_CLOSE, C2S_SURFACE_INPUT, C2S_SURFACE_POINTER,
    C2S_SURFACE_POINTER_AXIS,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::Receive;
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const GATHER_TIMEOUT: Duration = Duration::from_secs(4);

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

/// Result from a single parallel gather task.
enum GatherResult {
    Srflx { srflx: SocketAddr, base: SocketAddr },
    Relay(TurnRelay),
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
    // --- Bind sockets ---
    let udp4 = UdpSocket::bind("0.0.0.0:0")?;
    udp4.set_nonblocking(true)?;
    let port4 = udp4.local_addr()?.port();
    let tokio_udp4 = tokio::net::UdpSocket::from_std(udp4)?;

    // IPv6 socket is optional — skip silently if the OS doesn't support it.
    let udp6_result = UdpSocket::bind("[::]:0").and_then(|s| {
        s.set_nonblocking(true)?;
        Ok(s)
    });
    let (tokio_udp6, port6): (Option<tokio::net::UdpSocket>, Option<u16>) = match udp6_result
        .and_then(|s| {
            let port = s.local_addr()?.port();
            Ok((tokio::net::UdpSocket::from_std(s)?, port))
        }) {
        Ok((s, p)) => (Some(s), Some(p)),
        Err(_) => (None, None),
    };

    // --- Resolve local IPs and compute host addresses ---
    let local_ips = crate::default_local_ips();
    let host_addr4: SocketAddr = local_ips
        .iter()
        .find(|ip| ip.is_ipv4())
        .map(|ip| SocketAddr::new(*ip, port4))
        .unwrap_or_else(|| SocketAddr::new("0.0.0.0".parse::<IpAddr>().unwrap(), port4));
    let host_addr6: Option<SocketAddr> = tokio_udp6.as_ref().and(
        local_ips
            .iter()
            .find(|ip| ip.is_ipv6())
            .map(|ip| SocketAddr::new(*ip, port6.unwrap_or(0))),
    );

    // --- Build Rtc and add host candidates ---
    let mut rtc = Rtc::new(Instant::now());

    if let Ok(c) = Candidate::host(host_addr4, "udp") {
        verbose!("host candidate (IPv4): {host_addr4}");
        rtc.add_local_candidate(c);
    }
    if let Some(h6) = host_addr6
        && let Ok(c) = Candidate::host(h6, "udp")
    {
        verbose!("host candidate (IPv6): {h6}");
        rtc.add_local_candidate(c);
    }

    // --- Parallel ICE gathering ---
    let mut relay: Option<TurnRelay> = None;

    if let Some(config) = &ice_config {
        let (stun_servers, turn_servers) = ice::collect_servers(config);

        let mut tasks: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = Option<GatherResult>> + Send>>,
        > = FuturesUnordered::new();

        // STUN binding on IPv4 — use host_addr4 as the base so the srflx
        // candidate's base port matches the main tokio_udp4 socket used for
        // transmit routing.
        for stun_addr in stun_servers.iter().copied() {
            let base4 = host_addr4;
            tasks.push(Box::pin(async move {
                let udp = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => s,
                    Err(_) => return None,
                };
                match turn::stun_binding(stun_addr, &udp).await {
                    Ok(srflx) => Some(GatherResult::Srflx { srflx, base: base4 }),
                    Err(e) => {
                        verbose!("STUN binding failed ({stun_addr}): {e}");
                        None
                    }
                }
            }));
        }

        // STUN binding on IPv6 — use host_addr6 as the base for the same reason.
        if let Some(base6) = host_addr6 {
            for stun_addr in stun_servers.iter().copied() {
                tasks.push(Box::pin(async move {
                    let udp = match tokio::net::UdpSocket::bind("[::]:0").await {
                        Ok(s) => s,
                        Err(_) => return None,
                    };
                    match turn::stun_binding(stun_addr, &udp).await {
                        Ok(srflx) => {
                            if !srflx.ip().is_ipv6() {
                                return None;
                            }
                            Some(GatherResult::Srflx { srflx, base: base6 })
                        }
                        Err(e) => {
                            verbose!("STUN binding (IPv6) failed ({stun_addr}): {e}");
                            None
                        }
                    }
                }));
            }
        }

        // TURN allocations — all in parallel
        for ts in turn_servers.iter().cloned() {
            tasks.push(Box::pin(async move {
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
                        verbose!(
                            "TURN allocated relay {} via {:?} {}",
                            r.relay_addr,
                            ts.transport,
                            ts.addr
                        );
                        Some(GatherResult::Relay(r))
                    }
                    Err(e) => {
                        verbose!("TURN allocate ({:?} {}) failed: {e}", ts.transport, ts.addr);
                        None
                    }
                }
            }));
        }

        // Drain results until timeout, stop trying TURN once we have a relay.
        let deadline = tokio::time::sleep(GATHER_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                result = tasks.next() => {
                    match result {
                        None => break, // all tasks done
                        Some(None) => {} // this task failed, keep going
                        Some(Some(GatherResult::Srflx { srflx, base })) => {
                            if let Ok(c) = Candidate::server_reflexive(srflx, base, "udp") {
                                verbose!("srflx candidate: {srflx} (base {base})");
                                rtc.add_local_candidate(c);
                            }
                        }
                        Some(Some(GatherResult::Relay(r))) => {
                            if relay.is_none() {
                                if let Ok(c) = Candidate::relayed(r.relay_addr, host_addr4, "udp") {
                                    verbose!("relay candidate: {}", r.relay_addr);
                                    rtc.add_local_candidate(c);
                                }
                                relay = Some(r);
                                // Don't break — let STUN tasks finish, but
                                // remaining TURN tasks will just be dropped when
                                // tasks goes out of scope after the loop.
                            }
                        }
                    }
                }
                _ = &mut deadline => {
                    verbose!("ICE gathering timed out after {}s", GATHER_TIMEOUT.as_secs());
                    break;
                }
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

    verbose!("consumer access: {:?}", consumer_access);

    let answer = rtc.sdp_api().accept_offer(offer)?;
    let answer_json = serde_json::to_value(&answer)?;
    let signal_data = serde_json::json!({ "sdp": answer_json });
    let bk = keys.box_keys_for(consumer_access);
    let msg = signaling::build_sealed_message(&keys.signing, &peer_session_id, &signal_data, &bk);
    signal_tx
        .send(msg)
        .map_err(|e| format!("send failed: {e}"))?;

    let mut buf4 = vec![0u8; 65535];
    let mut buf6 = vec![0u8; 65535];
    let mut signaling_alive = true;

    let relay_addr = relay.as_ref().map(|r| r.relay_addr);

    // Per-channel state: abort handle for the pump task + tx to send
    // DataChannel→blit-server data into the task.
    struct ChannelState {
        abort: tokio::task::AbortHandle,
        /// DataChannel → blit-server: send framed payload bytes.
        write_tx: mpsc::UnboundedSender<Vec<u8>>,
    }
    let mut channels: HashMap<ChannelId, ChannelState> = HashMap::new();

    // blit-server → DataChannel: pump tasks send (ChannelId, framed data) here.
    let (server_tx, mut server_rx) = mpsc::unbounded_channel::<(ChannelId, Vec<u8>)>();

    loop {
        let timeout = loop {
            match rtc.poll_output()? {
                Output::Timeout(v) => break v,
                Output::Transmit(t) => {
                    if relay_addr == Some(t.source) {
                        if let Some(r) = &relay {
                            let _ = r.send_tx.send((t.destination, t.contents.to_vec()));
                        }
                    } else if host_addr6.map(|h6| h6 == t.source).unwrap_or(false) {
                        if let Some(ref udp6) = tokio_udp6 {
                            let _ = udp6.send_to(&t.contents, t.destination).await;
                        }
                    } else {
                        let _ = tokio_udp4.send_to(&t.contents, t.destination).await;
                    }
                    continue;
                }
                Output::Event(ev) => {
                    match ev {
                        Event::ChannelOpen(cid, label) => {
                            verbose!("data channel opened: {label}");
                            if label == "blit" {
                                // Connect a fresh Unix socket to blit-server for this channel.
                                #[cfg(unix)]
                                let ipc_result = tokio::net::UnixStream::connect(&sock_path)
                                    .await
                                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                                        e.into()
                                    });
                                #[cfg(windows)]
                                let ipc_result =
                                    tokio::net::windows::named_pipe::ClientOptions::new()
                                        .open(&sock_path)
                                        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                                            e.into()
                                        });

                                match ipc_result {
                                    Err(e) => {
                                        verbose!(
                                            "blit-server connect failed for channel {cid:?}: {e}"
                                        );
                                    }
                                    Ok(mut conn) => {
                                        established.store(true, Ordering::Relaxed);

                                        let (write_tx, mut write_rx) =
                                            mpsc::unbounded_channel::<Vec<u8>>();
                                        let tx = server_tx.clone();
                                        let access = consumer_access;

                                        let handle = tokio::spawn(async move {
                                            let mut len_buf = [0u8; 4];
                                            loop {
                                                tokio::select! {
                                                    // blit-server → DataChannel
                                                    read_result = conn.read_exact(&mut len_buf) => {
                                                        if read_result.is_err() { break; }
                                                        let len = u32::from_le_bytes(len_buf) as usize;
                                                        if len > MAX_FRAME_SIZE { break; }
                                                        let mut payload = vec![0u8; len];
                                                        if len > 0 && conn.read_exact(&mut payload).await.is_err() {
                                                            break;
                                                        }
                                                        let mut frame = Vec::with_capacity(4 + len);
                                                        frame.extend_from_slice(&(len as u32).to_le_bytes());
                                                        frame.extend_from_slice(&payload);
                                                        if tx.send((cid, frame)).is_err() { break; }
                                                    }
                                                    // DataChannel → blit-server
                                                    incoming = write_rx.recv() => {
                                                        match incoming {
                                                            Some(data) => {
                                                                // Enforce read-only before writing to server.
                                                                if access == crate::Access::ReadOnly
                                                                    && !data.is_empty()
                                                                    && is_write_message(data[0])
                                                                {
                                                                    continue;
                                                                }
                                                                let frame_len = (data.len() as u32).to_le_bytes();
                                                                if conn.write_all(&frame_len).await.is_err() { break; }
                                                                if !data.is_empty() && conn.write_all(&data).await.is_err() { break; }
                                                            }
                                                            None => break,
                                                        }
                                                    }
                                                }
                                            }
                                        });

                                        channels.insert(
                                            cid,
                                            ChannelState {
                                                abort: handle.abort_handle(),
                                                write_tx,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                        Event::ChannelData(cd) => {
                            // Forward payload to the per-channel pump task.
                            if let Some(state) = channels.get(&cd.id) {
                                let data = &cd.data;
                                if data.len() < 4 {
                                    continue;
                                }
                                let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]])
                                    as usize;
                                if data.len() < 4 + len {
                                    continue;
                                }
                                let payload = data[4..4 + len].to_vec();
                                let _ = state.write_tx.send(payload);
                            }
                        }
                        Event::ChannelClose(cid) => {
                            verbose!("blit data channel closed, keeping session alive");
                            if let Some(state) = channels.remove(&cid) {
                                state.abort.abort();
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
            result = tokio_udp4.recv_from(&mut buf4) => {
                let (n, source) = result?;
                if let Ok(receive) = Receive::new(
                    str0m::net::Protocol::Udp,
                    source,
                    host_addr4,
                    &buf4[..n],
                ) {
                    rtc.handle_input(Input::Receive(Instant::now(), receive))?;
                }
            }
            result = async {
                if let Some(ref udp6) = tokio_udp6 {
                    udp6.recv_from(&mut buf6).await
                } else {
                    std::future::pending().await
                }
            } => {
                let (n, source) = result?;
                if let Some(h6) = host_addr6
                    && let Ok(receive) = Receive::new(
                        str0m::net::Protocol::Udp,
                        source,
                        h6,
                        &buf6[..n],
                    )
                {
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
                    std::future::pending::<Option<(SocketAddr, Vec<u8>)>>().await
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
            // blit-server → DataChannel: pump tasks forward data here.
            msg = server_rx.recv() => {
                if let Some((cid, frame)) = msg
                    && let Some(mut ch) = rtc.channel(cid)
                {
                    let _ = ch.write(true, &frame);
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
                        let data = keys.open_sealed(&raw)
                            .map(|(v, _)| v)
                            .unwrap_or(raw);
                        if let Some(candidate) = data.get("candidate")
                            && let Ok(c) = serde_json::from_value::<Candidate>(candidate.clone())
                        {
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
