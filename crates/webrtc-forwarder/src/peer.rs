use crate::ProducerKeys;
use crate::ice::{self, IceConfig, Transport};
use crate::signaling;
use crate::turn::{self, TurnRelay};
use blit_remote::{
    C2S_CLIPBOARD_SET, C2S_CLOSE, C2S_CREATE, C2S_CREATE_AT, C2S_CREATE_N, C2S_CREATE2, C2S_INPUT,
    C2S_KILL, C2S_MOUSE, C2S_RESTART, C2S_SURFACE_CLOSE, C2S_SURFACE_INPUT, C2S_SURFACE_POINTER,
    C2S_SURFACE_POINTER_AXIS, S2C_QUIT,
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
use tokio::sync::{Notify, mpsc};

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const GATHER_TIMEOUT: Duration = Duration::from_secs(3);

/// Returns `true` for C2S message tags that mutate state (input, create, kill,
/// etc.).  Read-only consumers have these messages silently dropped.
fn is_write_message(tag: u8) -> bool {
    matches!(
        tag,
        C2S_INPUT
            | C2S_MOUSE
            | C2S_CLIPBOARD_SET
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

/// Mux control opcodes (stream_id=0).
const MUX_OPEN: u8 = 0x01;
const MUX_CLOSE: u8 = 0x02;

/// Per-virtual-stream state inside a mux channel.
struct MuxStreamState {
    abort: tokio::task::AbortHandle,
    /// DataChannel → blit-server: send raw payload bytes (no length prefix).
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// Result from a single parallel gather task.
enum GatherResult {
    Srflx { srflx: SocketAddr, base: SocketAddr },
    Relay(TurnRelay),
}

type BoxedRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
type BoxedWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;

/// Connect to the local blit-server, optionally routing through blit-proxy.
///
/// When `proxy_sock` is `Some`, the connection is established through the
/// blit-proxy daemon using the `target socket:<sock_path>\n` / `ok\n`
/// handshake.  Otherwise, a direct IPC connection is made.
///
/// If the proxy connection fails and `proxy_ensure` is provided, the proxy
/// daemon is restarted and the connection is retried once.
///
/// Returns boxed (reader, writer) halves ready for framed I/O.
async fn connect_to_server(
    sock_path: &str,
    proxy_sock: Option<&str>,
    proxy_ensure: Option<&crate::ProxyEnsureFn>,
) -> Result<(BoxedRead, BoxedWrite), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(proxy) = proxy_sock {
        match connect_via_proxy(proxy, sock_path).await {
            Ok(rw) => return Ok(rw),
            Err(first_err) => {
                // Proxy may be down — try to restart it and retry once.
                if let Some(ensure_fn) = proxy_ensure
                    && let Ok(new_sock) = ensure_fn().await
                {
                    verbose!("blit-proxy restarted → {new_sock}");
                    return connect_via_proxy(&new_sock, sock_path).await;
                }
                return Err(first_err);
            }
        }
    }

    #[cfg(unix)]
    {
        let conn = tokio::net::UnixStream::connect(sock_path).await?;
        let (r, w) = conn.into_split();
        Ok((Box::new(r), Box::new(w)))
    }
    #[cfg(windows)]
    {
        let conn = tokio::net::windows::named_pipe::ClientOptions::new().open(sock_path)?;
        let (r, w) = tokio::io::split(conn);
        Ok((Box::new(r), Box::new(w)))
    }
}

/// Connect to `sock_path` via the blit-proxy daemon at `proxy_sock`.
/// Performs the `target socket:<sock_path>\n` / `ok\n` handshake.
#[cfg(unix)]
async fn connect_via_proxy(
    proxy_sock: &str,
    sock_path: &str,
) -> Result<(BoxedRead, BoxedWrite), Box<dyn std::error::Error + Send + Sync>> {
    let mut stream = tokio::net::UnixStream::connect(proxy_sock).await?;
    let msg = format!("target socket:{sock_path}\n");
    stream.write_all(msg.as_bytes()).await?;

    // Read the handshake response byte-by-byte to avoid consuming data
    // past `ok\n` with a buffered reader.
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > 4096 {
            return Err("blit-proxy: handshake response too long".into());
        }
    }
    let resp = String::from_utf8_lossy(&buf);
    let resp = resp.trim_end_matches('\r');
    if resp == "ok" {
        let (r, w) = stream.into_split();
        Ok((Box::new(r), Box::new(w)))
    } else if let Some(m) = resp.strip_prefix("error ") {
        Err(format!("blit-proxy: {m}").into())
    } else {
        Err(format!("blit-proxy: unexpected response: {resp:?}").into())
    }
}

#[cfg(not(unix))]
async fn connect_via_proxy(
    _proxy_sock: &str,
    _sock_path: &str,
) -> Result<(BoxedRead, BoxedWrite), Box<dyn std::error::Error + Send + Sync>> {
    Err("blit-proxy is not supported on this platform".into())
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_peer(
    peer_session_id: String,
    sock_path: String,
    mut signal_rx: mpsc::UnboundedReceiver<serde_json::Value>,
    signal_tx: mpsc::UnboundedSender<String>,
    keys: ProducerKeys,
    established: Arc<AtomicBool>,
    ice_config: Option<IceConfig>,
    shutdown: Arc<Notify>,
    proxy_sock: Option<String>,
    proxy_ensure: Option<crate::ProxyEnsureFn>,
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

    // Idle detection: if we receive nothing from the peer for this long,
    // assume the connection is dead and tear down.  This catches cases where
    // ICE never transitions to Disconnected (e.g. TURN relay stays alive
    // after the browser tab is closed without clean teardown).
    const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
    // Stop pushing data into the DataChannel when no incoming peer data has
    // been observed for this long.  A healthy peer sends SCTP ACKs, ICE
    // binding responses, etc. — silence means the peer is gone.  Without
    // this gate the blit-server data pump keeps filling the SCTP send buffer,
    // generating a continuous stream of retransmission Transmit outputs that
    // spin the drive loop at full CPU.  After the cutoff, SCTP retransmits
    // only what is already buffered with natural RTO backoff.
    const SEND_IDLE_CUTOFF: Duration = Duration::from_secs(10);
    // Tear down peers whose DataChannels have all closed but whose ICE
    // is still "alive" (e.g. browser crashed, tab closed without clean
    // teardown — ICE consent / SCTP SACKs keep `last_peer_activity`
    // fresh so PEER_IDLE_TIMEOUT never fires, and str0m's drive loop
    // burns CPU processing ICE/DTLS/SCTP forever).  A healthy browser
    // reopens a channel immediately after closing one, so anything
    // longer than a few seconds without any channel means the peer is
    // really gone.
    const CHANNELS_EMPTY_TIMEOUT: Duration = Duration::from_secs(5);
    let mut last_peer_activity = Instant::now();
    let mut ever_had_channel = false;
    let mut channels_empty_since: Option<Instant> = None;

    // Reusable sleep future — avoids allocating/dropping a TimerEntry on every
    // loop iteration, which was responsible for ~15% of steady-state CPU
    // (timer wheel mutex contention + entry alloc/drop).
    let sleep = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(sleep);

    // Per-channel state: abort handle for the pump task + tx to send
    // DataChannel→blit-server data into the task.
    struct ChannelState {
        abort: tokio::task::AbortHandle,
        /// DataChannel → blit-server: send framed payload bytes.
        write_tx: mpsc::UnboundedSender<Vec<u8>>,
    }
    let mut channels: HashMap<ChannelId, ChannelState> = HashMap::new();

    // Frame parked when ch.write() returns Ok(false) — retried after
    // the next poll_output cycle processes SCTP acks and frees buffer.
    // The offset tracks how much of the frame has already been written
    // as separate SCTP messages (the browser reassembles them via its
    // readBuf accumulator).  This is critical because str0m caps the
    // SCTP send buffer at 128 KiB — a single large surface keyframe
    // (often 150-200 KiB) would permanently deadlock if sent as one
    // message since ch.write() rejects anything larger than available().
    let mut pending_send: Option<(ChannelId, Vec<u8>, usize)> = None;

    /// Maximum bytes per DataChannel write.  Must be well below str0m's
    /// MAX_BUFFERED_ACROSS_STREAMS (128 KiB) so that chunks fit even
    /// when the buffer isn't completely empty.
    const MAX_DC_CHUNK: usize = 64 * 1024;

    // Mux channels: maps DataChannel ID → per-stream state.
    let mut mux_channels: HashMap<ChannelId, HashMap<u16, MuxStreamState>> = HashMap::new();

    // blit-server → DataChannel: pump tasks send (ChannelId, framed data) here.
    // Bounded(1): when the DataChannel is congested and pending_send is
    // set, the drive loop stops reading server_rx.  The channel fills (1
    // slot), the pump task blocks on send(), which stops IPC reads, which
    // fills the kernel socket buffer, which backpressures the blit-server.
    // This prevents stale frames from piling up during congestion.
    let (server_tx, mut server_rx) = mpsc::channel::<(ChannelId, Vec<u8>)>(1);

    loop {
        // Check idle timeout before doing any work.
        if last_peer_activity.elapsed() > PEER_IDLE_TIMEOUT {
            verbose!(
                "peer idle for >{}s, tearing down",
                PEER_IDLE_TIMEOUT.as_secs()
            );
            break;
        }
        if let Some(t) = channels_empty_since
            && t.elapsed() > CHANNELS_EMPTY_TIMEOUT
        {
            verbose!(
                "all data channels closed for >{}s, tearing down",
                CHANNELS_EMPTY_TIMEOUT.as_secs()
            );
            break;
        }

        let timeout = loop {
            // After every poll_output step, try to flush pending_send.
            // Transmit outputs free SCTP send-buffer space, so retrying
            // here gives the parked frame the earliest chance to go out.
            // Writes are chunked at MAX_DC_CHUNK to avoid permanently
            // stalling on frames larger than the 128 KiB SCTP buffer.
            if let Some((ref cid, ref frame, ref mut offset)) = pending_send {
                if let Some(mut ch) = rtc.channel(*cid) {
                    while *offset < frame.len() {
                        let end = (*offset + MAX_DC_CHUNK).min(frame.len());
                        if matches!(ch.write(true, &frame[*offset..end]), Ok(true)) {
                            *offset = end;
                        } else {
                            break;
                        }
                    }
                    if *offset >= frame.len() {
                        pending_send = None;
                    }
                } else {
                    // Channel gone — connection is broken.
                    return Ok(());
                }
            }
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
                            ever_had_channel = true;
                            channels_empty_since = None;
                            if label == "mux" {
                                // Mux channel: virtual streams are opened
                                // via control messages on stream_id=0.
                                established.store(true, Ordering::Relaxed);
                                mux_channels.insert(cid, HashMap::new());
                            } else if label == "blit" {
                                // Legacy single-stream channel.
                                let ipc_result = connect_to_server(
                                    &sock_path,
                                    proxy_sock.as_deref(),
                                    proxy_ensure.as_ref(),
                                )
                                .await;

                                match ipc_result {
                                    Err(e) => {
                                        verbose!(
                                            "blit-server connect failed for channel {cid:?}: {e}"
                                        );
                                    }
                                    Ok((mut conn_r, mut conn_w)) => {
                                        established.store(true, Ordering::Relaxed);

                                        let (write_tx, mut write_rx) =
                                            mpsc::unbounded_channel::<Vec<u8>>();
                                        let tx = server_tx.clone();
                                        let access = consumer_access;

                                        // Split into two independent tasks so
                                        // neither direction can cancel the
                                        // other mid-read/write.  A
                                        // tokio::select! over both directions
                                        // is unsound: cancelling read_exact
                                        // mid-stream desyncs the framing, and
                                        // cancelling a bounded send loses the
                                        // frame entirely.
                                        let read_handle = tokio::spawn(async move {
                                            let mut len_buf = [0u8; 4];
                                            loop {
                                                if conn_r.read_exact(&mut len_buf).await.is_err() {
                                                    break;
                                                }
                                                let len = u32::from_le_bytes(len_buf) as usize;
                                                if len > MAX_FRAME_SIZE {
                                                    break;
                                                }
                                                let mut payload = vec![0u8; len];
                                                if len > 0
                                                    && conn_r
                                                        .read_exact(&mut payload)
                                                        .await
                                                        .is_err()
                                                {
                                                    break;
                                                }
                                                let mut frame = Vec::with_capacity(4 + len);
                                                frame
                                                    .extend_from_slice(&(len as u32).to_le_bytes());
                                                frame.extend_from_slice(&payload);
                                                if tx.send((cid, frame)).await.is_err() {
                                                    break;
                                                }
                                            }
                                        });
                                        let write_handle = tokio::spawn(async move {
                                            while let Some(data) = write_rx.recv().await {
                                                if access == crate::Access::ReadOnly
                                                    && !data.is_empty()
                                                    && is_write_message(data[0])
                                                {
                                                    continue;
                                                }
                                                let frame_len = (data.len() as u32).to_le_bytes();
                                                if conn_w.write_all(&frame_len).await.is_err() {
                                                    break;
                                                }
                                                if !data.is_empty()
                                                    && conn_w.write_all(&data).await.is_err()
                                                {
                                                    break;
                                                }
                                            }
                                        });
                                        let handle = tokio::spawn(async move {
                                            // When either direction ends,
                                            // abort the other so the IPC
                                            // connection is fully torn down.
                                            tokio::select! {
                                                _ = read_handle => {}
                                                _ = write_handle => {}
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
                            if let Some(mux_streams) = mux_channels.get_mut(&cd.id) {
                                // Mux channel: demux by stream_id.
                                let data = &cd.data;
                                if data.len() < 4 {
                                    continue;
                                }
                                let outer_len =
                                    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
                                        as usize;
                                if data.len() < 4 + outer_len || outer_len < 2 {
                                    continue;
                                }
                                let payload = &data[4..4 + outer_len];
                                let stream_id = u16::from_le_bytes([payload[0], payload[1]]);
                                let inner = &payload[2..];

                                if stream_id == 0 {
                                    // Control message.
                                    if inner.len() >= 3 && inner[0] == MUX_OPEN {
                                        let sid = u16::from_le_bytes([inner[1], inner[2]]);
                                        verbose!("mux: OPEN stream {sid}");
                                        // Connect to blit-server for this virtual stream.
                                        let ipc_result = connect_to_server(
                                            &sock_path,
                                            proxy_sock.as_deref(),
                                            proxy_ensure.as_ref(),
                                        )
                                        .await;

                                        match ipc_result {
                                            Err(e) => {
                                                verbose!(
                                                    "mux: blit-server connect failed for stream {sid}: {e}"
                                                );
                                            }
                                            Ok((mut conn_r, mut conn_w)) => {
                                                let (write_tx, mut write_rx) =
                                                    mpsc::unbounded_channel::<Vec<u8>>();
                                                let tx = server_tx.clone();
                                                let access = consumer_access;
                                                let mux_cid = cd.id;
                                                let read_handle = tokio::spawn(async move {
                                                    let mut len_buf = [0u8; 4];
                                                    loop {
                                                        if conn_r
                                                            .read_exact(&mut len_buf)
                                                            .await
                                                            .is_err()
                                                        {
                                                            break;
                                                        }
                                                        let len =
                                                            u32::from_le_bytes(len_buf) as usize;
                                                        if len > MAX_FRAME_SIZE {
                                                            break;
                                                        }
                                                        let mut payload = vec![0u8; len];
                                                        if len > 0
                                                            && conn_r
                                                                .read_exact(&mut payload)
                                                                .await
                                                                .is_err()
                                                        {
                                                            break;
                                                        }
                                                        let total = 2 + 4 + len;
                                                        let mut frame =
                                                            Vec::with_capacity(4 + total);
                                                        frame.extend_from_slice(
                                                            &(total as u32).to_le_bytes(),
                                                        );
                                                        frame.extend_from_slice(&sid.to_le_bytes());
                                                        frame.extend_from_slice(&len_buf);
                                                        frame.extend_from_slice(&payload);
                                                        if tx.send((mux_cid, frame)).await.is_err()
                                                        {
                                                            break;
                                                        }
                                                    }
                                                });
                                                let write_handle = tokio::spawn(async move {
                                                    while let Some(data) = write_rx.recv().await {
                                                        if access == crate::Access::ReadOnly
                                                            && !data.is_empty()
                                                            && is_write_message(data[0])
                                                        {
                                                            continue;
                                                        }
                                                        let frame_len =
                                                            (data.len() as u32).to_le_bytes();
                                                        if conn_w
                                                            .write_all(&frame_len)
                                                            .await
                                                            .is_err()
                                                        {
                                                            break;
                                                        }
                                                        if !data.is_empty()
                                                            && conn_w
                                                                .write_all(&data)
                                                                .await
                                                                .is_err()
                                                        {
                                                            break;
                                                        }
                                                    }
                                                });
                                                let handle = tokio::spawn(async move {
                                                    tokio::select! {
                                                        _ = read_handle => {}
                                                        _ = write_handle => {}
                                                    }
                                                });
                                                mux_streams.insert(
                                                    sid,
                                                    MuxStreamState {
                                                        abort: handle.abort_handle(),
                                                        write_tx,
                                                    },
                                                );
                                            }
                                        }
                                    } else if inner.len() >= 3 && inner[0] == MUX_CLOSE {
                                        let sid = u16::from_le_bytes([inner[1], inner[2]]);
                                        verbose!("mux: CLOSE stream {sid}");
                                        if let Some(state) = mux_streams.remove(&sid) {
                                            state.abort.abort();
                                        }
                                    }
                                } else if let Some(state) = mux_streams.get(&stream_id) {
                                    // Data for a virtual stream — inner is the raw blit
                                    // frame bytes ([blit_len:u32][blit_payload]).  Strip the
                                    // length prefix and send the payload to the pump task
                                    // (which re-adds it when writing to blit-server).
                                    if inner.len() >= 4 {
                                        let blit_len = u32::from_le_bytes([
                                            inner[0], inner[1], inner[2], inner[3],
                                        ])
                                            as usize;
                                        if inner.len() >= 4 + blit_len {
                                            let blit_payload = inner[4..4 + blit_len].to_vec();
                                            let _ = state.write_tx.send(blit_payload);
                                        }
                                    }
                                }
                            } else if let Some(state) = channels.get(&cd.id) {
                                // Legacy (non-mux) channel.
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
                            verbose!("data channel closed");
                            if let Some(state) = channels.remove(&cid) {
                                state.abort.abort();
                            }
                            if let Some(mux_streams) = mux_channels.remove(&cid) {
                                for (_, state) in mux_streams {
                                    state.abort.abort();
                                }
                            }
                            if ever_had_channel && channels.is_empty() && mux_channels.is_empty() {
                                channels_empty_since.get_or_insert_with(Instant::now);
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

        let deadline = tokio::time::Instant::from_std(timeout);
        // Once the peer has gone quiet (no incoming UDP for
        // SEND_IDLE_CUTOFF), floor the sleep at 100 ms so SCTP
        // retransmit timers on already-buffered data cannot
        // busy-spin the event loop until PEER_IDLE_TIMEOUT tears
        // the connection down.
        let deadline = if last_peer_activity.elapsed() >= SEND_IDLE_CUTOFF {
            deadline.max(tokio::time::Instant::now() + Duration::from_millis(100))
        } else {
            deadline
        };
        sleep.as_mut().reset(deadline);

        tokio::select! {
            result = tokio_udp4.recv_from(&mut buf4) => {
                let (n, source) = result?;
                if let Ok(receive) = Receive::new(
                    str0m::net::Protocol::Udp,
                    source,
                    host_addr4,
                    &buf4[..n],
                ) {
                    last_peer_activity = Instant::now();
                    rtc.handle_input(Input::Receive(last_peer_activity, receive))?;
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
                    last_peer_activity = Instant::now();
                    rtc.handle_input(Input::Receive(last_peer_activity, receive))?;
                }
            }
            _ = &mut sleep => {
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
                    last_peer_activity = Instant::now();
                    rtc.handle_input(Input::Receive(last_peer_activity, receive))?;
                }
            }
            // blit-server → DataChannel: pump tasks forward data here.
            // Gated on recent peer activity so we don't spin the loop
            // writing into an SCTP association that can never deliver.
            // The blit-server paces frame delivery via its own goodput /
            // ACK feedback loop — the forwarder is a dumb reliable pipe.
            // If the SCTP send buffer is full, park the frame and retry
            // after the next poll_output cycle drains it.
            msg = async {
                if pending_send.is_some() {
                    return std::future::pending().await;
                }
                if last_peer_activity.elapsed() < SEND_IDLE_CUTOFF {
                    server_rx.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                if let Some((cid, frame)) = msg
                    && let Some(mut ch) = rtc.channel(cid) {
                        // Write in chunks so frames larger than the SCTP
                        // send buffer (128 KiB) make progress instead of
                        // permanently deadlocking.  The browser reassembles
                        // chunks via its readBuf length-prefix parser.
                        let mut offset = 0usize;
                        while offset < frame.len() {
                            let end = (offset + MAX_DC_CHUNK).min(frame.len());
                            if matches!(ch.write(true, &frame[offset..end]), Ok(true)) {
                                offset = end;
                            } else {
                                break;
                            }
                        }
                        if offset < frame.len() {
                            pending_send = Some((cid, frame, offset));
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
            _ = shutdown.notified() => {
                // Forwarder is shutting down — send S2C_QUIT through each
                // active data channel so the browser reconnects promptly.
                let quit_frame: &[u8] = &[1, 0, 0, 0, S2C_QUIT];
                for &cid in channels.keys() {
                    if let Some(mut ch) = rtc.channel(cid) {
                        let _ = ch.write(true, quit_frame);
                    }
                }
                // Flush the SCTP write: drain Transmit outputs so the quit
                // frame actually hits the wire before we tear down.
                while let Ok(Output::Transmit(t)) = rtc.poll_output() {
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
                }
                break;
            }
        }
    }

    // Clean up all channel pump tasks on exit.
    for (_, state) in channels {
        state.abort.abort();
    }
    for (_, mux_streams) in mux_channels {
        for (_, state) in mux_streams {
            state.abort.abort();
        }
    }
    Ok(())
}
