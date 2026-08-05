//! The client half of the `NET_*` relay, shared by `blit forward` and `blit socks`
//! (docs/design/net.md § Wire).
//!
//! One blit connection carries every relayed socket, so the connection state — the
//! id allocator, the demultiplexing table, and the byte pump that honours the
//! window — lives here rather than in either command. The two commands differ only
//! in where a target comes from: a spec for `forward`, an accepted request for
//! `socks`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::transport::{read_message, write_frame};
use blit_remote::net::{
    FEATURE_NET, NET_CLOSE_WRITE, NET_CLOSED_EOF, NET_MAX_CHUNK, NET_MAX_SOCKETS, NET_STATUS_OK,
    NET_WINDOW_BYTES, NET_WINDOW_MIN, NetOpen, S2C_NET_ACK, S2C_NET_CLOSED, S2C_NET_DATA,
    S2C_NET_DGRAM, S2C_NET_OPENED, msg_net_ack_c2s, msg_net_close, msg_net_data_c2s, msg_net_open,
    net_closed_text, net_status_text, parse_net_ack_s2c, parse_net_closed, parse_net_data_s2c,
    parse_net_dgram_s2c, parse_net_opened, parse_net_opened_window,
};
use blit_remote::{S2C_HELLO, S2C_QUIT, S2C_READY};

/// Loopback, and deliberately not `0.0.0.0`.
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// Bracket an IPv6 literal so the result re-parses as one field, not as several colon-separated ones.
pub fn bracket(addr: &str) -> String {
    if addr.contains(':') {
        format!("[{addr}]")
    } else {
        addr.to_string()
    }
}

// --------------------------------------------------------------------------- Shared connection state ---------------------------------------------------------------------------

/// What the reader task delivers to one relayed socket.
pub enum Event {
    Opened {
        status: u8,
        alpn: String,
        detail: String,
        /// The send window the server granted, absent from a server that does not report one.
        window: Option<u64>,
    },
    Data(Vec<u8>),
    Ack(u64),
    Closed {
        reason: u8,
        detail: String,
    },
}

/// How long a teardown gives a stream to finish on its own before leaving the
/// rest to the socket options. Long enough for a task that can run to run.
const TEAR_DOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

struct Socket {
    events: mpsc::UnboundedSender<Event>,
    /// Never sent on: dropping it is the signal, so it fires on any path that
    /// forgets this entry, teardown included.
    #[allow(dead_code)]
    cut: oneshot::Sender<()>,
    finished: oneshot::Receiver<()>,
}

/// The task side of a registered stream, held for as long as that task runs.
///
/// `cut` fires when the connection ends the stream, and is what a task parked
/// in a write to a local peer that has stopped reading selects on — dropping
/// the event sender does not cancel a write. Dropping the whole thing is how a
/// teardown learns the task is done.
pub struct Live {
    cut: oneshot::Receiver<()>,
    #[allow(dead_code)]
    finished: oneshot::Sender<()>,
}

/// Client-allocated stream ids plus the demultiplexing table (docs/design/net.md § Stream ids are client-allocated).
pub struct Conn {
    out: mpsc::UnboundedSender<Vec<u8>>,
    sockets: Mutex<HashMap<u16, Socket>>,
    next_id: Mutex<u16>,
}

impl Conn {
    /// Reserve an id and register a receiver for it.
    ///
    /// Public because a UDP flow drives itself: it has no window, no half-close,
    /// and one id per local source, so [`relay`] does not fit it.
    pub async fn open(&self, events: mpsc::UnboundedSender<Event>) -> Option<(u16, Live)> {
        let mut sockets = self.sockets.lock().await;
        if sockets.len() >= NET_MAX_SOCKETS {
            return None;
        }
        let mut next = self.next_id.lock().await;
        // Ids must not be reused while live; scan for the first free one.
        for _ in 0..=u16::MAX {
            let id = *next;
            *next = next.wrapping_add(1);
            if let std::collections::hash_map::Entry::Vacant(slot) = sockets.entry(id) {
                let (finished, done) = oneshot::channel();
                let (cut, cut_rx) = oneshot::channel();
                slot.insert(Socket {
                    events,
                    cut,
                    finished: done,
                });
                return Some((
                    id,
                    Live {
                        cut: cut_rx,
                        finished,
                    },
                ));
            }
        }
        None
    }

    pub async fn close(&self, id: u16) {
        self.sockets.lock().await.remove(&id);
    }

    pub fn send(&self, msg: Vec<u8>) {
        let _ = self.out.send(msg);
    }

    /// End every live stream, and wait for the tasks driving them.
    ///
    /// The connection dying is a truncation of every stream on it, but nothing
    /// on the wire says so: the reader just stops. Dropping the event senders is
    /// how that reaches each stream's own loop, and waiting for the `Live`
    /// handles is what keeps the caller from exiting the process before those
    /// loops have reset their local sockets — an exit closes them with a FIN,
    /// which is the truncation-as-success the reset exists to prevent.
    async fn tear_down(&self) {
        let sockets = std::mem::take(&mut *self.sockets.lock().await);
        // Every sender has to go before the first wait, or one slow stream holds
        // back the notification the others are still waiting for.
        let finished: Vec<_> = sockets.into_values().map(|sock| sock.finished).collect();
        // Bounded, because dropping a sender does not cancel a write: a stream
        // whose local peer stopped reading is parked in one, and shutdown must
        // not wait on a peer that may never read again. Those sockets still get
        // their reset from the zero linger, which the exit cannot undo.
        let _ = tokio::time::timeout(TEAR_DOWN_GRACE, async move {
            for wait in finished {
                let _ = wait.await;
            }
        })
        .await;
    }
}

/// Take over the transport: handshake, then hand back the connection every relayed socket rides.
///
/// The returned future is the reader, and it owns the rest of the process's life —
/// when the connection drops, every socket on it goes too. Callers spawn their
/// listeners and then await it.
pub async fn establish(
    transport: crate::transport::Transport,
) -> Result<(Arc<Conn>, impl std::future::Future<Output = ()>), String> {
    let (mut reader, mut writer) = transport.split();
    let mut pending = Vec::new();
    require_net(&mut reader, &mut pending).await?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let conn = Arc::new(Conn {
        out: out_tx,
        sockets: Mutex::new(HashMap::new()),
        next_id: Mutex::new(1),
    });

    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if !write_frame(&mut writer, &msg).await {
                break;
            }
        }
    });

    Ok((conn.clone(), reader_task(reader, pending, conn)))
}

/// Fan S2C messages out to the socket that owns each id.
async fn reader_task(
    mut reader: Box<dyn AsyncRead + Unpin + Send>,
    mut pending: Vec<u8>,
    conn: Arc<Conn>,
) {
    while let Some(msg) = read_message(&mut reader, &mut pending).await {
        if msg.is_empty() {
            continue;
        }
        let (id, event) = match msg[0] {
            S2C_NET_OPENED => match parse_net_opened(&msg) {
                Some((id, status, alpn, detail)) => (
                    id,
                    Event::Opened {
                        status,
                        alpn,
                        detail,
                        window: parse_net_opened_window(&msg),
                    },
                ),
                None => continue,
            },
            S2C_NET_DATA => match parse_net_data_s2c(&msg) {
                Some((id, data)) => (id, Event::Data(data.to_vec())),
                None => continue,
            },
            S2C_NET_DGRAM => match parse_net_dgram_s2c(&msg) {
                Some((id, data)) => (id, Event::Data(data.to_vec())),
                None => continue,
            },
            S2C_NET_ACK => match parse_net_ack_s2c(&msg) {
                Some((id, bytes)) => (id, Event::Ack(bytes)),
                None => continue,
            },
            S2C_NET_CLOSED => match parse_net_closed(&msg) {
                Some((id, reason, detail)) => (id, Event::Closed { reason, detail }),
                None => continue,
            },
            S2C_QUIT => {
                eprintln!("blit: server is shutting down");
                break;
            }
            _ => continue,
        };
        let sockets = conn.sockets.lock().await;
        if let Some(sock) = sockets.get(&id) {
            let _ = sock.events.send(event);
        }
    }
    conn.tear_down().await;
}

/// Handshake and refuse early if the server has no relay — an old server drops the opcode silently and every forward would hang on connect.
async fn require_net(
    reader: &mut (impl AsyncRead + Unpin),
    pending: &mut Vec<u8>,
) -> Result<(), String> {
    let mut features = 0u32;
    loop {
        let data = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_message(reader, pending),
        )
        .await
        .map_err(|_| "timeout waiting for server".to_string())?
        .ok_or_else(|| "server closed connection".to_string())?;
        if data.is_empty() {
            continue;
        }
        match data[0] {
            S2C_HELLO if data.len() >= 7 => {
                features = u32::from_le_bytes([data[3], data[4], data[5], data[6]]);
            }
            S2C_QUIT => return Err("server is shutting down".into()),
            S2C_READY => {
                if features & FEATURE_NET == 0 {
                    return Err(
                        "server does not support port forwarding (upgrade blit on the remote)"
                            .into(),
                    );
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------------- The pump ---------------------------------------------------------------------------

/// What a stream assumes when its server does not report the window it granted.
///
/// Every stream starts at [`NET_WINDOW_MIN`] and adopts the reported figure on the
/// accept, so this decides only what an older server's silence means. It has to
/// stay a per-command choice: the shrinking window and its BUDGET close predate
/// the report, so against such a server a command that opens many sockets is one
/// overrun away from a killed stream, while one that opens a couple would pay for
/// that caution on every byte.
#[derive(Clone, Copy)]
pub enum Unreported {
    /// `NET_WINDOW_BYTES`, the share of a connection carrying at most four sockets.
    Ceiling,
    /// `NET_WINDOW_MIN`, the smallest share the server ever hands out, and so the
    /// only figure safe at any concurrency.
    Floor,
}

impl Unreported {
    fn bytes(self) -> u64 {
        match self {
            Unreported::Ceiling => NET_WINDOW_BYTES,
            Unreported::Floor => NET_WINDOW_MIN,
        }
    }
}

/// What the local client is owed when the server answers the open.
pub enum OnOpen {
    /// Nothing: a forward relays the target's own protocol, so the local client
    /// has no idea a relay is involved and a failed open is only an error message.
    /// `announce_alpn` says the negotiated protocol once per listener, for `tls/`.
    Report {
        announce_alpn: Option<Arc<AtomicBool>>,
    },
    /// A handshake reply built from the status, written before any relayed byte.
    /// SOCKS5 needs this: its client sends nothing until the CONNECT is answered,
    /// so the pump cannot start by pipelining the request the way a forward does.
    Answer(fn(u8) -> Vec<u8>),
}

/// Open one stream and relay bytes between it and `local` until either end closes.
///
/// `open.stream_id` is overwritten with the id this allocates.
pub async fn relay(
    local: tokio::net::TcpStream,
    conn: Arc<Conn>,
    mut open: NetOpen,
    unreported: Unreported,
    on_open: OnOpen,
) -> Result<(), String> {
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let Some((id, mut live)) = conn.open(events_tx).await else {
        // The cap is the connection's, not the socket's, so this is worth naming
        // rather than reporting as a connect failure against the target.
        if let OnOpen::Answer(reply) = &on_open {
            let mut local = local;
            let _ = local
                .write_all(&reply(blit_remote::net::NET_STATUS_BUDGET))
                .await;
        }
        return Err("too many relayed sockets".into());
    };
    let _ = local.set_nodelay(true);

    open.stream_id = id;
    let target = format!("{}:{}", bracket(&open.host), open.port);
    conn.send(msg_net_open(&open));

    let (mut local_read, mut local_write) = local.into_split();
    // The server acks bytes it has written to the target; that is the client's credit signal for the direction it drives.
    let (ack_tx, mut ack_rx) = watch::channel(0u64);
    // How much may be in flight. The server grants each stream a share of the
    // connection's aggregate — below 1 MiB from the fifth concurrent socket on,
    // and overrunning it does not throttle the stream, it kills it — and it
    // reports the figure on the accept. Until that arrives, and from a server
    // too old to report one, assume the smallest share the server ever grants:
    // one round trip of pipelining at the floor costs nothing, and guessing the
    // ceiling truncates every stream past the fourth.
    let (window_tx, mut window_rx) = watch::channel(NET_WINDOW_MIN);

    // Read the local side from the start even when a reply is owed: pipelining the
    // request behind the open is what keeps a forward's connect to one round trip,
    // and a SOCKS client sends nothing before its reply, so there is nothing to
    // leak past it.
    let up_conn = conn.clone();
    let up = tokio::spawn(async move {
        let mut buf = vec![0u8; NET_MAX_CHUNK];
        let mut sent: u64 = 0;
        loop {
            // Read only what there is credit for. Waiting for a whole chunk's
            // worth would leave the local socket unread while credit sits
            // unused, and reading a whole chunk first would put a chunk more
            // than the window in flight — which is not throttled, it is closed.
            let room = loop {
                let inflight = sent.saturating_sub(*ack_rx.borrow_and_update());
                let window = *window_rx.borrow_and_update();
                if window > inflight {
                    break ((window - inflight) as usize).min(NET_MAX_CHUNK);
                }
                tokio::select! {
                    changed = ack_rx.changed() => if changed.is_err() { return },
                    changed = window_rx.changed() => if changed.is_err() { return },
                }
            };
            match local_read.read(&mut buf[..room]).await {
                Ok(0) | Err(_) => {
                    // Local client is done writing: half-close, so a target that reads to EOF sees one.
                    up_conn.send(msg_net_close(id, NET_CLOSE_WRITE));
                    return;
                }
                Ok(n) => {
                    sent += n as u64;
                    up_conn.send(msg_net_data_c2s(id, &buf[..n]));
                }
            }
        }
    });

    let mut opened = false;
    let mut received: u64 = 0;
    // A FIN says "that was all of it", which a local client reading to the
    // close cannot tell apart from a complete transfer. Every end but the
    // target's own EOF has to reset instead.
    let mut truncated = false;
    let result = loop {
        let Some(event) = events.recv().await else {
            // The connection went away mid-stream without a NET_CLOSED.
            truncated = true;
            break Ok(());
        };
        match event {
            Event::Opened {
                status,
                alpn,
                detail,
                window,
            } => {
                if let OnOpen::Answer(reply) = &on_open
                    && local_write.write_all(&reply(status)).await.is_err()
                {
                    break Ok(());
                }
                if status != NET_STATUS_OK {
                    let detail = if detail.is_empty() {
                        net_status_text(status).to_string()
                    } else {
                        format!("{}: {detail}", net_status_text(status))
                    };
                    break Err(format!("{target}: {detail}"));
                }
                // A server that reports nothing is older than the grant, but it
                // enforces the same shrinking window all the same, so what its
                // silence may be read as is the caller's call (`Unreported`).
                let _ = window_tx.send(window.unwrap_or(unreported.bytes()));
                if let OnOpen::Report {
                    announce_alpn: Some(announced),
                } = &on_open
                    && !announced.swap(true, Ordering::Relaxed)
                {
                    let how = if alpn.is_empty() {
                        "no alpn".to_string()
                    } else {
                        format!("alpn={alpn}")
                    };
                    eprintln!("blit: tls to {target} established ({how})");
                }
                opened = true;
            }
            Event::Data(data) => {
                let wrote = tokio::select! {
                    written = local_write.write_all(&data) => written.is_ok(),
                    // The connection ended while this write was parked, which it
                    // is whenever the local peer has stopped reading. Nothing is
                    // going to drain it, and the stream is cut either way.
                    _ = &mut live.cut => {
                        truncated = true;
                        false
                    }
                };
                if !wrote {
                    break Ok(());
                }
                received += data.len() as u64;
                conn.send(msg_net_ack_c2s(id, received));
            }
            Event::Ack(bytes) => {
                let _ = ack_tx.send(bytes);
            }
            Event::Closed { reason, detail } => {
                if !detail.is_empty() {
                    eprintln!("blit: stream {id} {}: {detail}", net_closed_text(reason));
                }
                if reason == NET_CLOSED_EOF {
                    let _ = local_write.shutdown().await;
                } else {
                    truncated = true;
                }
                break Ok(());
            }
        }
    };
    up.abort();
    if truncated {
        // A zero linger makes the close an RST, and `forget` suppresses the FIN
        // that dropping the half would otherwise send ahead of it.
        let _ = local_write.as_ref().set_zero_linger();
        local_write.forget();
    }
    if opened {
        conn.send(msg_net_close(id, 0));
    }
    conn.close(id).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// Drive one `relay` against a scripted server side: a local TCP client to
    /// write into, the C2S messages the pump produced, and the events channel
    /// the reader task would normally feed.
    struct Pump {
        client: Option<tokio::net::TcpStream>,
        out: mpsc::UnboundedReceiver<Vec<u8>>,
        events: Option<mpsc::UnboundedSender<Event>>,
        /// The transport's other end and the real `reader_task` on this one, as
        /// an unspawned future — `cmd_forward` awaits it inline, and spawning it
        /// here would add a scheduler yield production does not have.
        #[allow(clippy::type_complexity)]
        transport: Option<(
            tokio::io::DuplexStream,
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        )>,
        _conn: Arc<Conn>,
    }

    /// Read a local socket to its end, whatever that end turns out to be.
    async fn local_end(mut client: tokio::net::TcpStream) -> (Vec<u8>, io::Result<()>) {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(500), client.read(&mut buf))
                .await
            {
                Ok(Ok(0)) => return (seen, Ok(())),
                Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
                Ok(Err(e)) => return (seen, Err(e)),
                Err(_) => {
                    return (
                        seen,
                        Err(io::Error::new(io::ErrorKind::TimedOut, "still open")),
                    );
                }
            }
        }
    }

    impl Pump {
        async fn start(unreported: Unreported) -> Pump {
            let (out_tx, mut out) = mpsc::unbounded_channel();
            let conn = Arc::new(Conn {
                out: out_tx,
                sockets: Mutex::new(HashMap::new()),
                next_id: Mutex::new(1),
            });
            let (server_side, our_side) = tokio::io::duplex(1024);
            let reader = Box::pin(reader_task(Box::new(our_side), Vec::new(), conn.clone()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (local, _) = listener.accept().await.unwrap();
            let pumped = conn.clone();
            tokio::spawn(async move {
                let _ = relay(
                    local,
                    pumped,
                    NetOpen::tcp(0, "target.internal", 80),
                    unreported,
                    OnOpen::Report {
                        announce_alpn: None,
                    },
                )
                .await;
            });
            // The open goes out before anything else, and its id is the one the
            // events channel is keyed on.
            let open = out.recv().await.unwrap();
            let id = blit_remote::net::parse_net_open(&open).unwrap().stream_id;
            let events = conn.sockets.lock().await.get(&id).unwrap().events.clone();
            Pump {
                client: Some(client),
                out,
                events: Some(events),
                transport: Some((server_side, reader)),
                _conn: conn,
            }
        }

        /// Write more than any window on offer, as a local client that never
        /// reads its own socket does, and hold it open — an EOF would end the
        /// pump for reasons unrelated to pacing.
        fn flood(&mut self) {
            let mut client = self.client.take().unwrap();
            tokio::spawn(async move {
                let _ = client.write_all(&vec![b'u'; 4 * 1024 * 1024]).await;
                std::future::pending::<()>().await;
            });
        }

        /// One S2C event, as the reader would have fanned it out.
        fn send(&self, event: Event) {
            self.events.as_ref().unwrap().send(event).unwrap();
        }

        fn accepted(&self, window: Option<u64>) {
            self.send(Event::Opened {
                status: NET_STATUS_OK,
                alpn: String::new(),
                detail: String::new(),
                window,
            });
        }

        /// What the local client sees on its own socket: the relayed bytes, and
        /// then either a clean end or the error a reset produces.
        async fn local_end(&mut self) -> (Vec<u8>, io::Result<()>) {
            local_end(self.client.take().unwrap()).await
        }

        /// The transport dying, driven through the real reader: drop the far
        /// end and await the task, exactly as `cmd_forward` does. The fixture's
        /// own event sender goes first, so the connection holds the only one.
        async fn connection_lost(&mut self) {
            self.events = None;
            let (far_end, reader) = self.transport.take().unwrap();
            drop(far_end);
            reader.await;
        }

        /// Let the relay task run until it has nothing left to do, which for a
        /// stalled peer means parked inside its write.
        async fn settle(&mut self) {
            while tokio::time::timeout(std::time::Duration::from_millis(50), self.out.recv())
                .await
                .is_ok()
            {}
        }

        /// Bytes the pump sends until it goes quiet, which is the only way to ask
        /// "has it stopped at the window" of a pump that never errors.
        async fn drain(&mut self) -> u64 {
            let mut bytes = 0;
            while let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(100), self.out.recv()).await
            {
                if let Some((_, data)) = blit_remote::net::parse_net_data_c2s(&msg) {
                    bytes += data.len() as u64;
                }
            }
            bytes
        }
    }

    /// Sending stopped inside the window, and stopped because of it: a client
    /// that used only half of what it was granted would pass an upper bound
    /// alone while costing throughput.
    #[track_caller]
    fn fills(sent: u64, window: u64) {
        assert!(sent <= window, "sent {sent} into a {window}-byte window");
        assert!(
            sent + NET_MAX_CHUNK as u64 > window,
            "stopped at {sent} with a {window}-byte window open"
        );
    }

    /// The client used to assume `NET_WINDOW_BYTES` for every stream, and the
    /// server kills a stream that exceeds the smaller share it actually
    /// granted — silently, since the local writer's send succeeded.
    #[tokio::test]
    async fn upload_stays_inside_the_window_it_was_granted() {
        let mut pump = Pump::start(Unreported::Ceiling).await;
        pump.flood();

        // Before the accept lands the client cannot know its share, so it
        // pipelines only what the server always grants.
        let mut sent = pump.drain().await;
        fills(sent, NET_WINDOW_MIN);

        // A fifth concurrent socket's share, the case that used to truncate.
        let granted = 838_860u64;
        pump.accepted(Some(granted));
        sent += pump.drain().await;
        fills(sent, granted);

        // And credit means more, not a fresh window: the counter is cumulative.
        pump.send(Event::Ack(granted / 2));
        sent += pump.drain().await;
        fills(sent, granted + granted / 2);
    }

    /// A reported window overrides the caller's guess in both directions: a
    /// proxy that would have assumed the floor uses the whole grant.
    #[tokio::test]
    async fn a_reported_window_overrides_a_floor_guess() {
        let mut pump = Pump::start(Unreported::Floor).await;
        pump.flood();
        let mut sent = pump.drain().await;
        fills(sent, NET_WINDOW_MIN);
        pump.accepted(Some(NET_WINDOW_BYTES));
        sent += pump.drain().await;
        fills(sent, NET_WINDOW_BYTES);
    }

    /// A server too old to report a window enforces one all the same, and what
    /// its silence means is the caller's call: a forward would rather have the
    /// throughput, a proxy holding dozens of sockets would rather not be closed.
    #[tokio::test]
    async fn an_unreported_window_is_the_callers_guess() {
        for (unreported, expected) in [
            (Unreported::Ceiling, NET_WINDOW_BYTES),
            (Unreported::Floor, NET_WINDOW_MIN),
        ] {
            let mut pump = Pump::start(unreported).await;
            pump.flood();
            let mut sent = pump.drain().await;
            fills(sent, NET_WINDOW_MIN);
            pump.accepted(None);
            sent += pump.drain().await;
            fills(sent, expected);
        }
    }

    /// A stream cut short must not look finished. Every reason but the target's
    /// own EOF means the local client did not get everything, and a FIN is
    /// indistinguishable from a complete transfer for anything that reads to the
    /// close. `SHUTDOWN` is in the list because it is in the wire spec, not
    /// because this repo's server sends it — a real shutdown is `S2C_QUIT`,
    /// which `a_connection_that_goes_away_resets_every_local_socket` covers.
    #[tokio::test]
    async fn an_abnormal_close_resets_the_local_socket() {
        for reason in [
            blit_remote::net::NET_CLOSED_RESET,
            blit_remote::net::NET_CLOSED_BUDGET,
            blit_remote::net::NET_CLOSED_POLICY,
            blit_remote::net::NET_CLOSED_SHUTDOWN,
        ] {
            let mut pump = Pump::start(Unreported::Ceiling).await;
            pump.accepted(Some(NET_WINDOW_BYTES));
            pump.send(Event::Data(b"half a response".to_vec()));
            pump.send(Event::Closed {
                reason,
                detail: String::new(),
            });
            let (_, end) = pump.local_end().await;
            let err = end.expect_err("a cut stream ended the local socket cleanly");
            assert_eq!(
                err.kind(),
                io::ErrorKind::ConnectionReset,
                "reason {reason}: {err}"
            );
        }
    }

    /// The connection going away truncates every stream on it, and says so on
    /// none of them: the reader just stops. It has to reach the local sockets
    /// all the same, and before the caller exits the process — which is why
    /// `tear_down` waits rather than only dropping the senders.
    #[tokio::test]
    async fn a_connection_that_goes_away_resets_every_local_socket() {
        let mut pump = Pump::start(Unreported::Ceiling).await;
        pump.accepted(Some(NET_WINDOW_BYTES));
        pump.send(Event::Data(b"half a response".to_vec()));
        pump.drain().await;

        pump.connection_lost().await;
        // Asserted without awaiting anything else: the parting close is only
        // here if `tear_down` waited for the task, which is the difference
        // between resetting the socket and racing the process's exit.
        let parting = pump
            .out
            .try_recv()
            .expect("tear_down returned before the stream's task had finished");
        assert_eq!(parting[0], blit_remote::net::C2S_NET_CLOSE);

        let (_, end) = pump.local_end().await;
        let err = end.expect_err("a lost connection ended the local socket cleanly");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
    }

    /// A local peer that has stopped reading parks the relay inside a write,
    /// which dropping its event sender does not cancel. The teardown must still
    /// end, and that stream must still be reset rather than left to the FIN a
    /// process exit would send.
    #[tokio::test]
    async fn a_stalled_local_peer_does_not_hold_up_the_teardown() {
        let mut pump = Pump::start(Unreported::Ceiling).await;
        pump.accepted(Some(NET_WINDOW_BYTES));
        // More than any socket buffer will take from a client that never reads.
        pump.send(Event::Data(vec![b'd'; 8 * 1024 * 1024]));
        pump.settle().await;

        tokio::time::timeout(TEAR_DOWN_GRACE / 2, pump.connection_lost())
            .await
            .expect("teardown waited on a peer that had stopped reading");

        let (_, end) = pump.local_end().await;
        let err = end.expect_err("a stalled peer's stream ended cleanly");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err}");
    }

    /// And the clean case stays clean: a target that closed on its own is an
    /// ordinary end of stream, and every byte of it must arrive.
    #[tokio::test]
    async fn a_target_eof_ends_the_local_socket_cleanly() {
        let mut pump = Pump::start(Unreported::Ceiling).await;
        pump.accepted(Some(NET_WINDOW_BYTES));
        pump.send(Event::Data(b"a whole response".to_vec()));
        pump.send(Event::Closed {
            reason: NET_CLOSED_EOF,
            detail: String::new(),
        });
        let (seen, end) = pump.local_end().await;
        end.expect("a target EOF must not reset the local socket");
        assert_eq!(seen, b"a whole response");
    }

    #[test]
    fn bracket_only_wraps_ipv6() {
        assert_eq!(bracket("127.0.0.1"), "127.0.0.1");
        assert_eq!(bracket("example.com"), "example.com");
        assert_eq!(bracket("::1"), "[::1]");
    }
}
