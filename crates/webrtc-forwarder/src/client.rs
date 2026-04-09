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
/// Maximum time to wait for the share producer (forwarder) to appear on the
/// signaling hub.  If the producer is offline this prevents the client from
/// blocking indefinitely.
const PEER_JOIN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    role: Option<String>,
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
    /// Open a new DataChannel with the given label and hand back a DuplexStream.
    Open {
        label: String,
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
    /// Establish the ICE+DTLS+SCTP session and open the first DataChannel
    /// labeled `"blit"`.
    /// This is the expensive part; subsequent `open_channel()` calls are cheap.
    pub async fn establish(
        passphrase: &str,
        signal_url: &str,
    ) -> Result<(Session, ChannelHandle, tokio::io::DuplexStream), BoxError> {
        Self::establish_with_label(passphrase, signal_url, "blit").await
    }

    /// Like [`establish`] but with a caller-chosen DataChannel label.
    pub async fn establish_with_label(
        passphrase: &str,
        signal_url: &str,
        label: &str,
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
        ) = setup_rtc(passphrase, signal_url, label).await?;

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
        self.open_channel_with_label("blit").await
    }

    /// Open a lightweight keepalive DataChannel.
    ///
    /// The channel label is `"keepalive"`, which the forwarder (producer)
    /// recognises and does **not** bridge to the blit-server, so no
    /// server-side client state is created.  The channel's only purpose is
    /// to keep the ICE/DTLS/SCTP session alive while the entry sits in a
    /// connection pool.
    pub async fn open_keepalive(&self) -> Result<ChannelHandle, String> {
        let (handle, _stream) = self.open_channel_with_label("keepalive").await?;
        // _stream dropped: the per-channel pump task exits, but the SCTP
        // channel remains open in str0m, keeping the session alive.
        Ok(handle)
    }

    /// Verify the ICE/DTLS/SCTP path is alive by doing a DCEP
    /// round-trip.  Opens a lightweight channel (not bridged to
    /// blit-server) and immediately closes it.  Returns `Ok(())` if
    /// the path is healthy.
    pub async fn probe(&self) -> Result<(), String> {
        let (handle, _stream) = self.open_channel_with_label("probe").await?;
        self.close_channel(handle);
        Ok(())
    }

    /// Open a DataChannel with an arbitrary label.
    async fn open_channel_with_label(
        &self,
        label: &str,
    ) -> Result<(ChannelHandle, tokio::io::DuplexStream), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .cmd_tx
            .send(DriveCmd::Open {
                label: label.to_string(),
                reply: reply_tx,
            })
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

    /// Returns `true` if the background drive task is still running.
    ///
    /// A session becomes non-alive when the drive task exits (ICE disconnect,
    /// error, etc.).  This is a cheap check (no I/O) — it just tests whether
    /// the command channel's receiver has been dropped.
    pub fn is_alive(&self) -> bool {
        !self.inner.cmd_tx.is_closed()
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
// MuxSession — multiplexed virtual streams over a single DataChannel
// ---------------------------------------------------------------------------

/// Mux control opcodes (sent on stream_id=0).
const MUX_OPEN: u8 = 0x01;
const MUX_CLOSE: u8 = 0x02;

/// A multiplexed session that carries many virtual blit streams over a
/// single SCTP DataChannel.  Each virtual stream gets its own
/// [`tokio::io::DuplexStream`] that the proxy can hand to a downstream
/// client.
///
/// Wire format of each frame written to the DataChannel's DuplexStream:
/// ```text
/// [total_len: u32 LE][stream_id: u16 LE][inner payload ...]
/// ```
/// where `total_len = 2 + inner_payload.len()`.
///
/// Stream 0 is the control channel:
///   OPEN:  [3:u32 LE][0:u16 LE][0x01][stream_id: u16 LE]
///   CLOSE: [3:u32 LE][0:u16 LE][0x02][stream_id: u16 LE]
///
/// Streams >= 1 carry raw blit frames:
///   [2+blit_frame_len: u32 LE][stream_id: u16 LE][blit_frame ...]
#[derive(Clone)]
pub struct MuxSession {
    inner: Arc<MuxInner>,
}

struct MuxInner {
    /// Session handle — kept alive to prevent the drive task from exiting.
    _session: Session,
    /// Send side of the mux DataChannel's DuplexStream.
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Requests to register a new stream's sender.
    reg_tx: mpsc::UnboundedSender<MuxReg>,
    /// Next stream ID to assign (starts at 1; 0 is control).
    next_id: std::sync::atomic::AtomicU16,
    /// Set to `false` when the read demux task exits (DataChannel EOF/error).
    mux_alive: Arc<std::sync::atomic::AtomicBool>,
}

struct MuxReg {
    stream_id: u16,
    /// Sender for data arriving from the remote for this stream.
    data_tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl MuxSession {
    /// Establish a WebRTC session and open a single `"mux"` DataChannel.
    /// A background task demuxes incoming data to per-stream receivers.
    pub async fn establish(passphrase: &str, signal_url: &str) -> Result<MuxSession, BoxError> {
        crate::init_verbose();
        let (session, _handle, stream) =
            Session::establish_with_label(passphrase, signal_url, "mux").await?;

        let (read_half, mut write_half) = tokio::io::split(stream);

        // Channel for the mux write path: any task can send framed data.
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        // Channel for registering new streams with the read demux task.
        let (reg_tx, mut reg_rx) = mpsc::unbounded_channel::<MuxReg>();

        // Write pump: serialises all outgoing mux frames onto the DuplexStream.
        tokio::spawn(async move {
            while let Some(frame) = write_rx.recv().await {
                if write_half.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });

        let mux_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mux_alive2 = mux_alive.clone();

        // Read demux: reads mux frames from the DuplexStream and routes by stream_id.
        tokio::spawn(async move {
            let mut read_half = read_half;
            let mut streams: std::collections::HashMap<u16, mpsc::UnboundedSender<Vec<u8>>> =
                std::collections::HashMap::new();
            let mut len_buf = [0u8; 4];

            loop {
                // Drain any pending registrations before blocking on read.
                while let Ok(reg) = reg_rx.try_recv() {
                    streams.insert(reg.stream_id, reg.data_tx);
                }

                // Use biased select so the read branch is always
                // polled first.  This prevents the registration branch
                // from cancelling a partially-completed read_exact,
                // which would desync the framing.
                tokio::select! {
                    biased;
                    // Read the next mux frame.
                    read_result = read_half.read_exact(&mut len_buf) => {
                        if read_result.is_err() { break; }
                        let total_len = u32::from_le_bytes(len_buf) as usize;
                        if !(2..=MAX_FRAME_SIZE).contains(&total_len) { break; }
                        let mut payload = vec![0u8; total_len];
                        if read_half.read_exact(&mut payload).await.is_err() { break; }
                        let stream_id = u16::from_le_bytes([payload[0], payload[1]]);
                        let inner = &payload[2..];

                        if stream_id == 0 {
                            // Control message — handle CLOSE from producer.
                            if inner.len() >= 3 && inner[0] == MUX_CLOSE {
                                let closed_id = u16::from_le_bytes([inner[1], inner[2]]);
                                streams.remove(&closed_id);
                            }
                        } else if let Some(tx) = streams.get(&stream_id) {
                            // Forward inner payload (raw blit frame bytes) to stream.
                            let data: Vec<u8> = inner.to_vec();
                            if tx.send(data).is_err() {
                                streams.remove(&stream_id);
                            }
                        }
                    }
                    // Accept new stream registrations.
                    reg = reg_rx.recv() => {
                        match reg {
                            Some(r) => { streams.insert(r.stream_id, r.data_tx); }
                            None => break, // MuxSession dropped.
                        }
                    }
                }
            }
            // DataChannel is dead — signal liveness check.
            mux_alive2.store(false, std::sync::atomic::Ordering::Relaxed);
        });

        Ok(MuxSession {
            inner: Arc::new(MuxInner {
                _session: session,
                write_tx,
                reg_tx,
                next_id: std::sync::atomic::AtomicU16::new(1),
                mux_alive,
            }),
        })
    }

    /// Open a new virtual stream.  This is a **local operation** — no
    /// network round-trip.  Returns a DuplexStream whose reads produce
    /// blit frames from the remote, and whose writes send blit frames
    /// to the remote.
    pub fn open_stream(&self) -> Result<(u16, tokio::io::DuplexStream), String> {
        let id = self
            .inner
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if id == 0 {
            // Wrapped around — this would be the control stream.
            return Err("stream ID space exhausted".into());
        }

        // Register a receiver for incoming data on this stream.
        let (data_tx, mut data_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.inner
            .reg_tx
            .send(MuxReg {
                stream_id: id,
                data_tx,
            })
            .map_err(|_| "mux session closed".to_string())?;

        // Create a DuplexStream pair — the app half goes to the caller,
        // the driver half is pumped by background tasks.
        let (app_half, driver_half) = tokio::io::duplex(256 * 1024);
        let (mut drv_read, mut drv_write) = tokio::io::split(driver_half);

        // Send OPEN control message.
        let mut open_frame = Vec::with_capacity(4 + 2 + 1 + 2);
        open_frame.extend_from_slice(&5u32.to_le_bytes()); // total_len = 2 + 3
        open_frame.extend_from_slice(&0u16.to_le_bytes()); // stream_id = 0 (control)
        open_frame.push(MUX_OPEN);
        open_frame.extend_from_slice(&id.to_le_bytes());
        let _ = self.inner.write_tx.send(open_frame);

        // Pump: remote → app (incoming blit frames).
        // data_rx delivers raw blit frame bytes (already stripped of mux header).
        // We write them as-is to the DuplexStream for the app to read with
        // read_frame().
        tokio::spawn(async move {
            while let Some(data) = data_rx.recv().await {
                if drv_write.write_all(&data).await.is_err() {
                    break;
                }
            }
            let _ = drv_write.shutdown().await;
        });

        // Pump: app → remote (outgoing blit frames).
        // Read blit frames from the app, wrap in mux framing, send.
        let write_tx = self.inner.write_tx.clone();
        let stream_id = id;
        tokio::spawn(async move {
            let mut len_buf = [0u8; 4];
            loop {
                if drv_read.read_exact(&mut len_buf).await.is_err() {
                    break;
                }
                let len = u32::from_le_bytes(len_buf) as usize;
                if len > MAX_FRAME_SIZE {
                    break;
                }
                let mut payload = vec![0u8; len];
                if len > 0 && drv_read.read_exact(&mut payload).await.is_err() {
                    break;
                }
                // Build mux frame: [total_len:u32 LE][stream_id:u16 LE][blit frame]
                let total = 2 + 4 + len;
                let mut frame = Vec::with_capacity(4 + total);
                frame.extend_from_slice(&(total as u32).to_le_bytes());
                frame.extend_from_slice(&stream_id.to_le_bytes());
                frame.extend_from_slice(&len_buf); // original blit length prefix
                frame.extend_from_slice(&payload);
                if write_tx.send(frame).is_err() {
                    break;
                }
            }
        });

        Ok((id, app_half))
    }

    /// Close a virtual stream.  Sends a CLOSE control message to the
    /// remote so it can tear down the corresponding blit-server
    /// connection.
    pub fn close_stream(&self, id: u16) {
        let mut frame = Vec::with_capacity(4 + 2 + 1 + 2);
        frame.extend_from_slice(&5u32.to_le_bytes()); // total_len = 2 + 3
        frame.extend_from_slice(&0u16.to_le_bytes()); // stream_id = 0 (control)
        frame.push(MUX_CLOSE);
        frame.extend_from_slice(&id.to_le_bytes());
        let _ = self.inner.write_tx.send(frame);
    }

    /// Returns `true` if both the drive task and the mux DataChannel
    /// are still alive.
    pub fn is_alive(&self) -> bool {
        self.inner._session.is_alive()
            && self
                .inner
                .mux_alive
                .load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Common setup: ICE gathering + SDP exchange
// ---------------------------------------------------------------------------

#[allow(clippy::type_complexity)]
async fn setup_rtc(
    passphrase: &str,
    signal_url: &str,
    channel_label: &str,
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
    let mut forwarder_session_id = tokio::time::timeout(PEER_JOIN_TIMEOUT, async {
        loop {
            let msg = ws_read
                .next()
                .await
                .ok_or("signaling closed before peer joined")?;
            let msg = msg?;
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
                    return Ok::<_, BoxError>(id);
                }
                if m.msg_type == "error" {
                    return Err(format!("signaling: {}", m.message.unwrap_or_default()).into());
                }
            }
        }
    })
    .await
    .map_err(|_| -> BoxError {
        "timed out waiting for share producer (is `blit share` running on the remote?)".into()
    })??;

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
    let first_cid = changes.add_channel(channel_label.to_string());
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

    // Reusable sleep future — avoids allocating/dropping a TimerEntry on every
    // loop iteration, which was responsible for ~15% of steady-state CPU
    // (timer wheel mutex contention + entry alloc/drop).
    let sleep = tokio::time::sleep(std::time::Duration::ZERO);
    tokio::pin!(sleep);

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

    let mut pending_send: Option<(ChannelId, Vec<u8>)> = None;

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
            if let Some((cid, ref frame)) = pending_send {
                if let Some(mut ch) = rtc.channel(cid) {
                    if matches!(ch.write(true, frame), Ok(true)) {
                        pending_send = None;
                    }
                } else {
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
                            verbose!("DataChannel opened: {label} (id {cid:?})");
                            if let Some(reply_tx) = pending_open.remove(&cid) {
                                let (app_half, driver_half) = tokio::io::duplex(256 * 1024 * 1024);
                                // write_tx: drive task → app half (DataChannel → app).
                                let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                                // app_tx: reader task → drive task (app → DataChannel).
                                let app_tx = app_data_tx.clone();
                                // Split the DuplexStream so each direction
                                // gets its own task — no select cancellation.
                                let (mut drv_r, mut drv_w) = tokio::io::split(driver_half);
                                let read_handle = tokio::spawn(async move {
                                    let mut len_buf = [0u8; 4];
                                    loop {
                                        if drv_r.read_exact(&mut len_buf).await.is_err() {
                                            break;
                                        }
                                        let len = u32::from_le_bytes(len_buf) as usize;
                                        if len > MAX_FRAME_SIZE {
                                            break;
                                        }
                                        let mut payload = vec![0u8; len];
                                        if len > 0 && drv_r.read_exact(&mut payload).await.is_err()
                                        {
                                            break;
                                        }
                                        let mut frame = Vec::with_capacity(4 + len);
                                        frame.extend_from_slice(&(len as u32).to_le_bytes());
                                        frame.extend_from_slice(&payload);
                                        if app_tx.send((cid, frame)).is_err() {
                                            break;
                                        }
                                    }
                                });
                                let write_handle = tokio::spawn(async move {
                                    while let Some(data) = write_rx.recv().await {
                                        if drv_w.write_all(&data).await.is_err() {
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
                            if !session_alive && channel_tasks.is_empty() && pending_open.is_empty()
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

        let deadline = tokio::time::Instant::from_std(timeout);
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
                    rtc.handle_input(Input::Receive(Instant::now(), receive))?;
                }
            }
            cmd = async {
                if session_alive { cmd_rx.recv().await } else { std::future::pending().await }
            } => {
                match cmd {
                    Some(DriveCmd::Open { label, reply }) => {
                        let mut changes = rtc.sdp_api();
                        let cid = changes.add_channel(label);
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
            // app → DataChannel: forward to SCTP.  If the send
            // buffer is full, park and retry next poll_output cycle.
            app_msg = async {
                if pending_send.is_some() {
                    return std::future::pending().await;
                }
                app_data_rx.recv().await
            } => {
                if let Some((id, data)) = app_msg
                    && let Some(mut ch) = rtc.channel(id)
                        && !matches!(ch.write(true, &data), Ok(true)) {
                            pending_send = Some((id, data));
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
