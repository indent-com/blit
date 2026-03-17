use crate::BoxKeys;
use crate::ice::{self, Transport};
use crate::signaling;
use crate::turn::{self, TurnRelay};
use futures_util::{
    SinkExt,
    stream::{FuturesUnordered, StreamExt},
};
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};
use str0m::channel::ChannelId;

/// Opaque handle to an open DataChannel on a [`Session`].
/// Returned by [`Session::open_channel`] and consumed by [`Session::close_channel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelHandle(ChannelId);
use str0m::net::Receive;
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
const GATHER_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Deserialize)]
#[allow(dead_code)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    role: Option<String>,
    from: Option<String>,
    data: Option<serde_json::Value>,
    message: Option<String>,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Result from a single parallel gather task.
enum GatherResult {
    Srflx { srflx: SocketAddr, base: SocketAddr },
    Relay(TurnRelay),
}

// ---------------------------------------------------------------------------
// Commands sent into the drive task
// ---------------------------------------------------------------------------

enum DriveCmd {
    /// Open a new "blit" DataChannel and hand back a DuplexStream.
    Open {
        reply: oneshot::Sender<Result<(ChannelId, tokio::io::DuplexStream), String>>,
    },
    /// Close a channel. The ICE/DTLS session keeps running.
    Close { id: ChannelId },
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A live WebRTC session (ICE+DTLS+SCTP) that can open and close DataChannels
/// without re-negotiating.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    cmd_tx: mpsc::UnboundedSender<DriveCmd>,
}

impl Session {
    /// Establish the ICE+DTLS+SCTP session and open the first DataChannel.
    /// This is the expensive part; subsequent `open_channel()` calls are cheap.
    pub async fn establish(
        passphrase: &str,
        signal_url: &str,
    ) -> Result<(Session, ChannelHandle, tokio::io::DuplexStream), BoxError> {
        crate::init_verbose();
        let (
            rtc,
            tokio_udp4,
            host_addr4,
            tokio_udp6,
            host_addr6,
            relay,
            ws_read,
            ws_write,
            box_keys,
            first_cid,
        ) = setup_rtc(passphrase, signal_url).await?;

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DriveCmd>();
        let (ready_tx, ready_rx) =
            oneshot::channel::<Result<(ChannelId, tokio::io::DuplexStream), String>>();

        tokio::spawn(async move {
            if let Err(e) = drive(
                rtc, tokio_udp4, host_addr4, tokio_udp6, host_addr6, relay, ws_read, ws_write,
                first_cid, ready_tx, cmd_rx, box_keys,
            )
            .await
            {
                verbose!("webrtc client error: {e}");
            }
        });

        let (cid, stream) = ready_rx
            .await
            .map_err(|_| "driver task died before first channel open")??;

        let session = Session {
            inner: Arc::new(SessionInner { cmd_tx }),
        };
        Ok((session, ChannelHandle(cid), stream))
    }

    /// Open a new "blit" DataChannel on the existing session.
    /// No ICE or SDP negotiation — just an SCTP stream open.
    pub async fn open_channel(&self) -> Result<(ChannelHandle, tokio::io::DuplexStream), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .cmd_tx
            .send(DriveCmd::Open { reply: reply_tx })
            .map_err(|_| "drive task has exited".to_string())?;
        let (cid, stream) = reply_rx
            .await
            .map_err(|_| "drive task died waiting for channel open".to_string())??;
        Ok((ChannelHandle(cid), stream))
    }

    /// Close a specific channel. The underlying ICE/DTLS session stays alive.
    pub fn close_channel(&self, handle: ChannelHandle) {
        let _ = self.inner.cmd_tx.send(DriveCmd::Close { id: handle.0 });
    }
}

// ---------------------------------------------------------------------------
// Backwards-compatible connect()
// ---------------------------------------------------------------------------

/// Establish a session and open a channel. The session is dropped after this
/// call, so the drive task exits once the channel closes — same behaviour as
/// before the Session API was introduced.
pub async fn connect(
    passphrase: &str,
    signal_url: &str,
) -> Result<tokio::io::DuplexStream, BoxError> {
    let (_session, _handle, stream) = Session::establish(passphrase, signal_url).await?;
    // _session dropped: cmd_tx dropped. Drive task exits when channel closes.
    Ok(stream)
}

// ---------------------------------------------------------------------------
// Common setup: ICE gathering + SDP exchange
// ---------------------------------------------------------------------------

#[allow(clippy::type_complexity)]
async fn setup_rtc(
    passphrase: &str,
    signal_url: &str,
) -> Result<
    (
        Rtc,
        tokio::net::UdpSocket,
        SocketAddr,
        Option<tokio::net::UdpSocket>,
        Option<SocketAddr>,
        Option<TurnRelay>,
        futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
        Option<BoxKeys>,
        ChannelId,
    ),
    BoxError,
> {
    let consumer =
        crate::parse_consumer_secret(passphrase).map_err(|e| -> BoxError { e.into() })?;
    // Connect to the producer's channel (the passphrase-derived Ed25519 public
    // key) and sign with the matching secret key.  The hub verifies signatures
    // against the channel ID as the Ed25519 public key, so the signing key must
    // correspond to the channel we connect to.
    // Multiple consumers can coexist in the same channel; the hub gives each a
    // unique sessionId (UUID).
    let signing_key = consumer.signing.clone();
    let public_key_hex = crate::hex_encode(signing_key.verifying_key().as_bytes());
    let box_keys = Some(consumer.box_keys());

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
        if let Message::Text(t) = msg
            && let Ok(m) = serde_json::from_str::<ServerMessage>(&t)
        {
            if m.msg_type == "registered" {
                let id = m.session_id.unwrap_or_default();
                verbose!("registered with signaling hub (session {id})");
                break id;
            }
            if m.msg_type == "error" {
                return Err(format!("signaling: {}", m.message.unwrap_or_default()).into());
            }
        }
    };

    verbose!("waiting for forwarder to join signaling hub...");
    let mut forwarder_session_id = loop {
        let msg = ws_read
            .next()
            .await
            .ok_or("signaling closed before peer joined")??;
        if let Message::Text(t) = msg
            && let Ok(m) = serde_json::from_str::<ServerMessage>(&t)
        {
            if m.msg_type == "peer_joined" {
                // Only accept peer_joined from the producer side; ignore
                // other consumers that may join the same channel (e.g. other
                // gateway connections to the same share: remote).
                if m.role.as_deref() == Some("consumer") {
                    verbose!("ignoring peer_joined from another consumer");
                    continue;
                }
                let id = m.session_id.unwrap_or_default();
                verbose!("forwarder joined (session {id})");
                break id;
            }
            if m.msg_type == "error" {
                return Err(format!("signaling: {}", m.message.unwrap_or_default()).into());
            }
        }
    };

    let udp4 = UdpSocket::bind("0.0.0.0:0")?;
    udp4.set_nonblocking(true)?;
    let port4 = udp4.local_addr()?.port();
    let tokio_udp4 = tokio::net::UdpSocket::from_std(udp4)?;

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

    let mut relay: Option<TurnRelay> = None;

    if let Some(config) = &ice_config {
        let (stun_servers, turn_servers) = ice::collect_servers(config);

        let mut tasks: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = Option<GatherResult>> + Send>>,
        > = FuturesUnordered::new();

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

        let deadline = tokio::time::sleep(GATHER_TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                result = tasks.next() => {
                    match result {
                        None => break,
                        Some(None) => {}
                        Some(Some(GatherResult::Srflx { srflx, base })) => {
                            if let Ok(c) = Candidate::server_reflexive(srflx, base, "udp") {
                                verbose!("srflx candidate: {srflx} (base {base})");
                                rtc.add_local_candidate(c);
                            }
                        }
                        Some(Some(GatherResult::Relay(r))) => {
                            if relay.is_none() {
                                if let Ok(c) =
                                    Candidate::relayed(r.relay_addr, host_addr4, "udp")
                                {
                                    verbose!("relay candidate: {}", r.relay_addr);
                                    rtc.add_local_candidate(c);
                                }
                                relay = Some(r);
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

    // First channel — triggers the SDP offer/answer.
    let mut changes = rtc.sdp_api();
    let first_cid = changes.add_channel("blit".to_string());
    let (offer, pending) = changes.apply().unwrap();

    let offer_json = serde_json::to_value(&offer)?;
    let signal_data = serde_json::json!({ "sdp": offer_json });
    let msg = match &box_keys {
        Some(bk) => {
            signaling::build_sealed_message(&signing_key, &forwarder_session_id, &signal_data, bk)
        }
        None => signaling::build_signed_message(&signing_key, &forwarder_session_id, &signal_data),
    };
    verbose!("sending SDP offer to forwarder...");
    ws_write.send(Message::Text(msg.into())).await?;

    let mut answer_pending = Some(pending);
    let mut signal_rx_buf: Vec<serde_json::Value> = Vec::new();

    loop {
        let msg = ws_read
            .next()
            .await
            .ok_or("signaling closed before answer")??;
        if let Message::Text(t) = msg
            && let Ok(m) = serde_json::from_str::<ServerMessage>(&t)
        {
            verbose!("signaling rx: type={:?}", m.msg_type);
            if m.msg_type == "peer_joined" {
                // Only react to producer peer_joined; ignore other consumers
                // joining the same channel.
                if m.role.as_deref() == Some("consumer") {
                    verbose!("ignoring peer_joined from another consumer during SDP exchange");
                    continue;
                }
                // The hub replaced the pairing (e.g. the ephemeral session
                // expired while we were doing ICE gathering).  Update our
                // target and re-send the offer so the forwarder can answer.
                let new_id = m.session_id.unwrap_or_default();
                if new_id != forwarder_session_id {
                    verbose!(
                        "forwarder session changed {forwarder_session_id} → {new_id}, re-sending SDP offer"
                    );
                    forwarder_session_id = new_id;
                    let offer_json = serde_json::to_value(&offer)?;
                    let signal_data = serde_json::json!({ "sdp": offer_json });
                    let msg = match &box_keys {
                        Some(bk) => signaling::build_sealed_message(
                            &signing_key,
                            &forwarder_session_id,
                            &signal_data,
                            bk,
                        ),
                        None => signaling::build_signed_message(
                            &signing_key,
                            &forwarder_session_id,
                            &signal_data,
                        ),
                    };
                    ws_write.send(Message::Text(msg.into())).await?;
                }
            } else if m.msg_type == "signal"
                && let Some(raw) = m.data
            {
                let data = box_keys
                    .as_ref()
                    .and_then(|bk| signaling::open_sealed_data(&raw, bk))
                    .unwrap_or(raw);
                if let Some(sdp) = data.get("sdp") {
                    let sdp_type = sdp.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                    verbose!("received SDP from forwarder: type={sdp_type:?}");
                    // The hub may echo our own offer back to us as a signal;
                    // ignore any SDP that isn't an answer.
                    match serde_json::from_value(sdp.clone()) {
                        Ok(answer) => {
                            if let Some(p) = answer_pending.take() {
                                rtc.sdp_api().accept_answer(p, answer)?;
                            }
                        }
                        Err(e) => {
                            verbose!("ignoring SDP signal that is not an answer: {e}");
                        }
                    }
                } else if data.get("candidate").is_some() {
                    verbose!("received remote ICE candidate (pre-answer buffer)");
                    signal_rx_buf.push(data);
                } else {
                    verbose!("received unknown signal data (ignored)");
                    signal_rx_buf.push(data);
                }
            }
            if answer_pending.is_none() {
                break;
            }
        }
    }

    verbose!(
        "applying {} buffered remote ICE candidates",
        signal_rx_buf.len()
    );
    for data in signal_rx_buf.drain(..) {
        if let Some(candidate) = data.get("candidate")
            && let Ok(c) = serde_json::from_value::<Candidate>(candidate.clone())
        {
            verbose!("remote ICE candidate: {c:?}");
            rtc.add_remote_candidate(c);
        }
    }

    Ok((
        rtc, tokio_udp4, host_addr4, tokio_udp6, host_addr6, relay, ws_read, ws_write, box_keys,
        first_cid,
    ))
}

// ---------------------------------------------------------------------------
// Drive task
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn drive(
    mut rtc: Rtc,
    tokio_udp4: tokio::net::UdpSocket,
    host_addr4: SocketAddr,
    tokio_udp6: Option<tokio::net::UdpSocket>,
    host_addr6: Option<SocketAddr>,
    mut relay: Option<TurnRelay>,
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
    first_cid: ChannelId,
    first_ready: oneshot::Sender<Result<(ChannelId, tokio::io::DuplexStream), String>>,
    mut cmd_rx: mpsc::UnboundedReceiver<DriveCmd>,
    box_keys: Option<BoxKeys>,
) -> Result<(), BoxError> {
    let relay_addr = relay.as_ref().map(|r| r.relay_addr);
    let mut buf4 = vec![0u8; 65535];
    let mut buf6 = vec![0u8; 65535];
    let mut signaling_alive = true;
    // Whether the Session handle is still alive (cmd_rx not yet closed).
    // Once false we stop polling cmd_rx but keep running until all channels close.
    let mut session_alive = true;

    // Separate channel for per-channel pump tasks to send app data back to
    // the drive loop, so we don't need to hold a cmd_tx clone (which would
    // prevent cmd_rx from ever returning None on Session drop).
    let (app_data_tx, mut app_data_rx) = mpsc::unbounded_channel::<(ChannelId, Vec<u8>)>();

    // Channels waiting for ChannelOpen confirmation.
    type PendingOpenMap = std::collections::HashMap<
        ChannelId,
        oneshot::Sender<Result<(ChannelId, tokio::io::DuplexStream), String>>,
    >;
    let mut pending_open: PendingOpenMap = std::collections::HashMap::new();
    pending_open.insert(first_cid, first_ready);

    // Active channels: per-channel (abort handle, write-tx for DataChannel→app).
    struct ChannelState {
        abort: tokio::task::AbortHandle,
        /// Sender for data arriving from the remote (DataChannel → app half).
        write_tx: mpsc::UnboundedSender<Vec<u8>>,
    }
    let mut channel_tasks: std::collections::HashMap<ChannelId, ChannelState> =
        std::collections::HashMap::new();

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
                            verbose!("DataChannel opened: {label} (id {cid:?})");
                            if label == "blit"
                                && let Some(reply_tx) = pending_open.remove(&cid)
                            {
                                let (app_half, mut driver_half) = tokio::io::duplex(256 * 1024);
                                // write_tx: drive task → app half (DataChannel → app).
                                let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                                // app_tx: reader task → drive task (app → DataChannel).
                                let app_tx = app_data_tx.clone();
                                let handle = tokio::spawn(async move {
                                    let mut len_buf = [0u8; 4];
                                    loop {
                                        tokio::select! {
                                            // app → DataChannel
                                            read_result = driver_half.read_exact(&mut len_buf) => {
                                                if read_result.is_err() { break; }
                                                let len = u32::from_le_bytes(len_buf) as usize;
                                                if len > MAX_FRAME_SIZE { break; }
                                                let mut payload = vec![0u8; len];
                                                if len > 0 && driver_half.read_exact(&mut payload).await.is_err() {
                                                    break;
                                                }
                                                let mut frame = Vec::with_capacity(4 + len);
                                                frame.extend_from_slice(&(len as u32).to_le_bytes());
                                                frame.extend_from_slice(&payload);
                                                if app_tx.send((cid, frame)).is_err() {
                                                    break;
                                                }
                                            }
                                            // DataChannel → app
                                            incoming = write_rx.recv() => {
                                                match incoming {
                                                    Some(data) => {
                                                        if driver_half.write_all(&data).await.is_err() {
                                                            break;
                                                        }
                                                    }
                                                    None => break,
                                                }
                                            }
                                        }
                                    }
                                });
                                channel_tasks.insert(
                                    cid,
                                    ChannelState {
                                        abort: handle.abort_handle(),
                                        write_tx,
                                    },
                                );
                                let _ = reply_tx.send(Ok((cid, app_half)));
                            }
                        }
                        Event::ChannelData(cd) => {
                            if let Some(state) = channel_tasks.get(&cd.id) {
                                let _ = state.write_tx.send(cd.data.to_vec());
                            }
                        }
                        Event::ChannelClose(cid) => {
                            if let Some(state) = channel_tasks.remove(&cid) {
                                state.abort.abort();
                            }
                            if let Some(tx) = pending_open.remove(&cid) {
                                let _ = tx.send(Err("channel closed before open".into()));
                            }
                            // If the Session was already dropped and this was
                            // the last channel, there is nothing left to do.
                            if !session_alive
                                && channel_tasks.is_empty()
                                && pending_open.is_empty()
                            {
                                return Ok(());
                            }
                        }
                        Event::IceConnectionStateChange(state) => {
                            verbose!("ICE state: {state:?}");
                            if matches!(state, str0m::IceConnectionState::Disconnected) {
                                for (_, tx) in pending_open.drain() {
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
            cmd = async {
                if session_alive { cmd_rx.recv().await } else { std::future::pending().await }
            } => {
                match cmd {
                    Some(DriveCmd::Open { reply }) => {
                        let mut changes = rtc.sdp_api();
                        let cid = changes.add_channel("blit".to_string());
                        // For non-first channels apply() returns None — no SDP needed.
                        let _ = changes.apply();
                        pending_open.insert(cid, reply);
                    }
                    Some(DriveCmd::Close { id }) => {
                        if let Some(state) = channel_tasks.remove(&id) {
                            state.abort.abort();
                        }
                        rtc.direct_api().close_data_channel(id);
                    }
                    None => {
                        // All Session handles dropped — no new channels will be
                        // requested, but keep running until existing channels close
                        // so in-flight data is not cut short (e.g. the backwards-
                        // compat connect() drops the Session immediately after
                        // opening the first channel).
                        session_alive = false;
                        if channel_tasks.is_empty() && pending_open.is_empty() {
                            return Ok(());
                        }
                    }
                }
            }
            // app → DataChannel: per-channel pump tasks forward data here.
            app_msg = app_data_rx.recv() => {
                if let Some((id, data)) = app_msg
                    && let Some(mut ch) = rtc.channel(id)
                {
                    let _ = ch.write(true, &data);
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
                        if let Ok(m) = serde_json::from_str::<ServerMessage>(&t)
                            && m.msg_type == "signal"
                            && let Some(raw) = m.data
                        {
                            let data = box_keys
                                .as_ref()
                                .and_then(|bk| signaling::open_sealed_data(&raw, bk))
                                .unwrap_or(raw);
                            if let Some(candidate) = data.get("candidate")
                                && let Ok(c) =
                                    serde_json::from_value::<Candidate>(candidate.clone())
                            {
                                verbose!("remote ICE candidate (trickle): {c:?}");
                                rtc.add_remote_candidate(c);
                            }
                        }
                    }
                    None | Some(Err(_)) => {
                        signaling_alive = false;
                    }
                    _ => {}
                }
            }
        }
    }
}
