//! `blit forward` — port forwarding over the blit connection (docs/design/net.md § Client: `blit forward`).
//! `ssh -L` over any blit transport, plus the UDP case ssh has never had.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, watch};

use crate::transport::{Transport, read_message, write_frame};
use blit_remote::net::{
    FEATURE_NET, NET_MAX_CHUNK, NET_MAX_DGRAM, NET_MAX_SOCKETS, NET_WINDOW_BYTES, NetOpen,
    S2C_NET_ACK, S2C_NET_CLOSED, S2C_NET_DATA, S2C_NET_DGRAM, S2C_NET_OPENED, msg_net_ack_c2s,
    msg_net_close, msg_net_data_c2s, msg_net_dgram_c2s, msg_net_open, net_closed_text,
    net_status_text, parse_net_ack_s2c, parse_net_closed, parse_net_data_s2c, parse_net_dgram_s2c,
    parse_net_opened,
};
use blit_remote::{S2C_HELLO, S2C_QUIT, S2C_READY};

// --------------------------------------------------------------------------- Specs ---------------------------------------------------------------------------

/// Which kind of socket a spec forwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Tcp,
    Udp,
    /// Local plaintext in, TLS to the target, terminated on the server.
    Tls,
}

/// One forward: a local listener and the target it relays to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub bind: String,
    pub local_port: u16,
    pub host: String,
    pub host_port: u16,
}

impl std::fmt::Display for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            Kind::Udp => write!(f, "udp/")?,
            Kind::Tls => write!(f, "tls/")?,
            Kind::Tcp => {}
        }
        if self.bind != DEFAULT_BIND {
            write!(f, "{}:", bracket(&self.bind))?;
        }
        write!(
            f,
            "{}:{}:{}",
            self.local_port,
            bracket(&self.host),
            self.host_port
        )
    }
}

/// Bracket an IPv6 literal so the result re-parses as one field, not as several colon-separated ones.
fn bracket(addr: &str) -> String {
    if addr.contains(':') {
        format!("[{addr}]")
    } else {
        addr.to_string()
    }
}

/// Loopback, and deliberately not `0.0.0.0`.
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// Parse `[kind/][bind_address:]local_port:host:host_port`.
pub fn parse_spec(s: &str) -> Result<Spec, String> {
    let bad = |what: &str| Err(format!("{s}: {what}"));
    let (kind, rest) = match s.split_once('/') {
        Some(("tcp", rest)) => (Kind::Tcp, rest),
        Some(("udp", rest)) => (Kind::Udp, rest),
        Some(("tls", rest)) => (Kind::Tls, rest),
        Some((other, _)) => {
            return bad(&format!("unknown kind `{other}` (want tcp, udp or tls)"));
        }
        None => (Kind::Tcp, s),
    };
    // Colon-separated fields, with `[...]` atomic so a bracketed IPv6 address is one field and not several.
    let Some(fields) = split_fields(rest) else {
        return bad("unterminated [address]");
    };
    let (bind, port_str, host, host_port_str) = match fields.as_slice() {
        [port, host, host_port] => (DEFAULT_BIND.to_string(), port, host, host_port),
        [bind, port, host, host_port] => (bind.clone(), port, host, host_port),
        _ => return bad("want [kind/][bind:]port:host:hostport"),
    };
    let host_port: u16 = match host_port_str.parse() {
        Ok(p) if p > 0 => p,
        _ => return bad(&format!("bad target port `{host_port_str}`")),
    };
    if host.is_empty() {
        return bad("empty target host");
    }
    // Port 0 means "pick one and tell me", which is what makes a forward scriptable without hunting for a free port first.
    let local_port: u16 = match port_str.parse() {
        Ok(p) => p,
        Err(_) => return bad(&format!("bad local port `{port_str}`")),
    };
    if bind.is_empty() {
        return bad("empty bind address");
    }
    Ok(Spec {
        kind,
        bind,
        local_port,
        host: host.clone(),
        host_port,
    })
}

/// Split on `:`, treating a leading `[...]` in each field as atomic so IPv6 literals survive.
fn split_fields(s: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_bracket = false;
    let mut closed = false;
    for c in s.chars() {
        match c {
            '[' if !in_bracket && cur.is_empty() && !closed => in_bracket = true,
            ']' if in_bracket => {
                in_bracket = false;
                closed = true;
            }
            ':' if !in_bracket => {
                fields.push(std::mem::take(&mut cur));
                closed = false;
            }
            // Trailing junk after `]` (as in `[::1]x:80`) is malformed, not silently concatenated.
            _ if closed => return None,
            _ => cur.push(c),
        }
    }
    if in_bracket {
        return None;
    }
    fields.push(cur);
    Some(fields)
}

/// TLS options for `tls/` specs.
#[derive(Clone, Debug, Default)]
pub struct TlsOpts {
    /// ALPN protocols to offer, in preference order.
    pub alpn: Vec<String>,
    /// Skip certificate verification.
    pub insecure: bool,
}

impl TlsOpts {
    /// The wire flags a `tls/` spec opens with.
    fn flags(&self) -> u8 {
        let mut flags = blit_remote::net::NET_OPEN_TLS;
        if self.insecure {
            flags |= blit_remote::net::NET_OPEN_INSECURE;
        }
        flags
    }
}

// --------------------------------------------------------------------------- Shared connection state ---------------------------------------------------------------------------

/// What the reader task delivers to one relayed socket.
enum Event {
    Opened {
        status: u8,
        alpn: String,
        detail: String,
    },
    Data(Vec<u8>),
    Ack(u64),
    Closed {
        reason: u8,
        detail: String,
    },
}

struct Socket {
    events: mpsc::UnboundedSender<Event>,
}

/// Client-allocated stream ids plus the demultiplexing table (docs/design/net.md § Stream ids are client-allocated).
struct Conn {
    out: mpsc::UnboundedSender<Vec<u8>>,
    sockets: Mutex<HashMap<u16, Socket>>,
    next_id: Mutex<u16>,
}

impl Conn {
    /// Reserve an id and register a receiver for it.
    async fn open(&self, events: mpsc::UnboundedSender<Event>) -> Option<u16> {
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
                slot.insert(Socket { events });
                return Some(id);
            }
        }
        None
    }

    async fn close(&self, id: u16) {
        self.sockets.lock().await.remove(&id);
    }

    fn send(&self, msg: Vec<u8>) {
        let _ = self.out.send(msg);
    }
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

// --------------------------------------------------------------------------- Entry point ---------------------------------------------------------------------------

/// Bind every listener, then serve.
pub async fn cmd_forward(
    transport: Transport,
    specs: Vec<Spec>,
    tls: TlsOpts,
) -> Result<i32, String> {
    if specs.is_empty() {
        return Err(
            "nothing to forward: pass a spec, or --all with entries in blit.forwards".into(),
        );
    }

    let mut tcp = Vec::new();
    let mut udp = Vec::new();
    for spec in &specs {
        let addr = format!("{}:{}", spec.bind, spec.local_port);
        match spec.kind {
            // A `tls/` forward listens in plaintext exactly like `tcp/`; the difference is one flag on the open.
            Kind::Tcp | Kind::Tls => {
                let listener = tokio::net::TcpListener::bind(&addr)
                    .await
                    .map_err(|e| format!("cannot bind {addr}: {e}"))?;
                tcp.push((spec.clone(), listener));
            }
            Kind::Udp => {
                let socket = tokio::net::UdpSocket::bind(&addr)
                    .await
                    .map_err(|e| format!("cannot bind {addr}/udp: {e}"))?;
                udp.push((spec.clone(), socket));
            }
        }
    }

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

    for (spec, listener) in tcp {
        let local = listener
            .local_addr()
            .map_err(|e| format!("cannot read local address: {e}"))?;
        report(&spec, local);
        let conn = conn.clone();
        let tls = tls.clone();
        tokio::spawn(async move { serve_tcp(listener, spec, conn, tls).await });
    }
    for (spec, socket) in udp {
        let local = socket
            .local_addr()
            .map_err(|e| format!("cannot read local address: {e}"))?;
        report(&spec, local);
        let conn = conn.clone();
        tokio::spawn(async move { serve_udp(socket, spec, conn).await });
    }

    // The reader owns the rest of the process's life: when the connection drops, every forward goes with it.
    reader_task(reader, pending, conn).await;
    Ok(0)
}

fn report(spec: &Spec, local: SocketAddr) {
    let kind = match spec.kind {
        Kind::Tcp => "tcp",
        Kind::Udp => "udp",
        Kind::Tls => "tcp → tls",
    };
    eprintln!(
        "blit: forwarding {kind} {local} → {}:{}",
        spec.host, spec.host_port
    );
}

// --------------------------------------------------------------------------- TCP ---------------------------------------------------------------------------

async fn serve_tcp(listener: tokio::net::TcpListener, spec: Spec, conn: Arc<Conn>, tls: TlsOpts) {
    // The negotiated protocol is worth saying once — per connection it would be noise on a busy forward.
    let announced = Arc::new(std::sync::atomic::AtomicBool::new(false));
    loop {
        let (local, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("blit: accept failed on {}: {e}", spec);
                return;
            }
        };
        let conn = conn.clone();
        let spec = spec.clone();
        let tls = tls.clone();
        let announced = announced.clone();
        tokio::spawn(async move {
            if let Err(e) = relay_tcp(local, spec, conn, tls, announced).await {
                eprintln!("blit: {peer}: {e}");
            }
        });
    }
}

async fn relay_tcp(
    local: tokio::net::TcpStream,
    spec: Spec,
    conn: Arc<Conn>,
    tls: TlsOpts,
    announced: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let Some(id) = conn.open(events_tx).await else {
        return Err("too many forwarded sockets".into());
    };
    let _ = local.set_nodelay(true);

    // Open and pipeline: the first bytes of the local client's request may ride in the same batch as the open, so nothing waits a round trip for an id the client already chose.
    let mut open = NetOpen::tcp(id, &spec.host, spec.host_port);
    if spec.kind == Kind::Tls {
        open.flags = tls.flags();
        open.alpn = tls.alpn.clone();
    }
    conn.send(msg_net_open(&open));

    let (mut local_read, mut local_write) = local.into_split();
    // The server acks bytes it has written to the target; that is the client's credit signal for the direction it drives.
    let (ack_tx, mut ack_rx) = watch::channel(0u64);

    let up_conn = conn.clone();
    let up = tokio::spawn(async move {
        let mut buf = vec![0u8; NET_MAX_CHUNK];
        let mut sent: u64 = 0;
        loop {
            while sent.saturating_sub(*ack_rx.borrow_and_update()) >= NET_WINDOW_BYTES {
                if ack_rx.changed().await.is_err() {
                    return;
                }
            }
            match local_read.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    // Local client is done writing: half-close, so a target that reads to EOF sees one.
                    up_conn.send(msg_net_close(id, blit_remote::net::NET_CLOSE_WRITE));
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
    let result = loop {
        let Some(event) = events.recv().await else {
            break Ok(());
        };
        match event {
            Event::Opened {
                status,
                alpn,
                detail,
            } => {
                if status != blit_remote::net::NET_STATUS_OK {
                    let detail = if detail.is_empty() {
                        net_status_text(status).to_string()
                    } else {
                        format!("{}: {detail}", net_status_text(status))
                    };
                    break Err(format!("{}:{}: {detail}", spec.host, spec.host_port));
                }
                if spec.kind == Kind::Tls
                    && !announced.swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    let how = if alpn.is_empty() {
                        "no alpn".to_string()
                    } else {
                        format!("alpn={alpn}")
                    };
                    eprintln!(
                        "blit: tls to {}:{} established ({how})",
                        spec.host, spec.host_port
                    );
                }
                opened = true;
            }
            Event::Data(data) => {
                if local_write.write_all(&data).await.is_err() {
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
                let _ = local_write.shutdown().await;
                break Ok(());
            }
        }
    };
    up.abort();
    if opened {
        conn.send(msg_net_close(id, 0));
    }
    conn.close(id).await;
    result
}

// --------------------------------------------------------------------------- UDP ---------------------------------------------------------------------------

/// One flow per distinct local source address, created on that source's first datagram and torn down by the server's idle timeout — the NAT model, because it is the only one that demultiplexes replies back to the right sender (docs/design/net.md § Client: `blit forward`).
async fn serve_udp(socket: tokio::net::UdpSocket, spec: Spec, conn: Arc<Conn>) {
    let socket = Arc::new(socket);
    let mut flows: HashMap<SocketAddr, mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
    let mut buf = vec![0u8; NET_MAX_DGRAM];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("blit: recv failed on {}: {e}", spec);
                return;
            }
        };
        // A closed flow leaves a dead sender behind; replace it rather than dropping the datagram, so a source that goes quiet past the idle timeout and comes back simply gets a new flow.
        let live = flows.get(&from).is_some_and(|tx| !tx.is_closed());
        if !live {
            match start_udp_flow(socket.clone(), from, &spec, conn.clone()).await {
                Some(tx) => {
                    flows.insert(from, tx);
                }
                None => {
                    eprintln!("blit: too many forwarded sockets, dropping datagram from {from}");
                    continue;
                }
            }
        }
        if let Some(tx) = flows.get(&from) {
            let _ = tx.send(buf[..n].to_vec());
        }
    }
}

/// Open one flow and spawn its pump.
async fn start_udp_flow(
    socket: Arc<tokio::net::UdpSocket>,
    from: SocketAddr,
    spec: &Spec,
    conn: Arc<Conn>,
) -> Option<mpsc::UnboundedSender<Vec<u8>>> {
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let id = conn.open(events_tx).await?;
    conn.send(msg_net_open(&NetOpen::udp(id, &spec.host, spec.host_port)));

    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let up_conn = conn.clone();
    let up = tokio::spawn(async move {
        while let Some(payload) = local_rx.recv().await {
            up_conn.send(msg_net_dgram_c2s(id, &payload));
        }
    });

    let target = format!("{}:{}", spec.host, spec.host_port);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                Event::Opened { status, detail, .. } => {
                    if status != blit_remote::net::NET_STATUS_OK {
                        let detail = if detail.is_empty() {
                            net_status_text(status).to_string()
                        } else {
                            format!("{}: {detail}", net_status_text(status))
                        };
                        eprintln!("blit: {target}: {detail}");
                        break;
                    }
                }
                Event::Data(payload) => {
                    // Reply demultiplexing: the flow knows which local source it belongs to, so the answer goes back there.
                    if socket.send_to(&payload, from).await.is_err() {
                        break;
                    }
                }
                Event::Closed { reason, detail } => {
                    if !detail.is_empty() {
                        eprintln!("blit: flow {id} {}: {detail}", net_closed_text(reason));
                    }
                    break;
                }
                Event::Ack(_) => {}
            }
        }
        up.abort();
        conn.close(id).await;
    });
    Some(local_tx)
}

// --------------------------------------------------------------------------- The named list ---------------------------------------------------------------------------

use blit_webserver::config::{ForwardEntry, modify_forwards, read_forwards_full};

/// Resolve what to forward: explicit specs, or every enabled entry in `blit.forwards` under `--all`.
pub fn resolve_specs(args: &[String], all: bool) -> Result<Vec<Spec>, String> {
    let mut specs = Vec::new();
    if all {
        for entry in read_forwards_full().into_iter().filter(|e| !e.disabled) {
            let spec = parse_spec(&entry.spec)
                .map_err(|e| format!("blit.forwards entry `{}`: {e}", entry.name))?;
            specs.push(spec);
        }
        if specs.is_empty() {
            return Err("no enabled entries in blit.forwards".into());
        }
    }
    for arg in args {
        specs.push(parse_spec(arg)?);
    }
    Ok(specs)
}

/// `blit forward add NAME SPEC` — add or update one entry.
pub fn cmd_add(name: &str, spec: &str) -> Result<i32, String> {
    if name.is_empty() || name.contains(char::is_whitespace) {
        return Err(format!("bad entry name `{name}`"));
    }
    // Validate before persisting: an entry that cannot parse is a `--all` that refuses to start, discovered much later.
    let parsed = parse_spec(spec)?;
    let stored = parsed.to_string();
    modify_forwards(|entries| {
        if let Some(existing) = entries.iter_mut().find(|e| e.name == name) {
            existing.spec = stored.clone();
            existing.disabled = false;
        } else {
            entries.push(ForwardEntry {
                name: name.to_string(),
                spec: stored.clone(),
                disabled: false,
            });
        }
    });
    println!("{name} = {stored}");
    Ok(0)
}

/// `blit forward list` — every entry, disabled ones marked.
pub fn cmd_list() -> Result<i32, String> {
    let entries = read_forwards_full();
    if entries.is_empty() {
        eprintln!("blit: no forwards configured (blit forward add NAME SPEC)");
        return Ok(0);
    }
    let width = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
    for e in entries {
        let mark = if e.disabled { " (disabled)" } else { "" };
        println!("{:<width$}  {}{mark}", e.name, e.spec, width = width);
    }
    Ok(0)
}

/// `blit forward rm NAME` — remove one entry.
pub fn cmd_rm(name: &str) -> Result<i32, String> {
    let before = read_forwards_full().len();
    modify_forwards(|entries| entries.retain(|e| e.name != name));
    if read_forwards_full().len() == before {
        return Err(format!("no such forward: {name}"));
    }
    Ok(0)
}

/// `blit forward toggle NAME` — disable or re-enable without removing, the `blit remote toggle` convention.
pub fn cmd_toggle(name: &str) -> Result<i32, String> {
    let mut found = false;
    modify_forwards(|entries| {
        if let Some(e) = entries.iter_mut().find(|e| e.name == name) {
            e.disabled = !e.disabled;
            found = true;
        }
    });
    if !found {
        return Err(format!("no such forward: {name}"));
    }
    cmd_list()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_tcp_spec() {
        let spec = parse_spec("8080:localhost:3000").unwrap();
        assert_eq!(
            spec,
            Spec {
                kind: Kind::Tcp,
                bind: DEFAULT_BIND.into(),
                local_port: 8080,
                host: "localhost".into(),
                host_port: 3000,
            }
        );
    }

    #[test]
    fn kind_prefixes() {
        assert_eq!(parse_spec("udp/53:r:53").unwrap().kind, Kind::Udp);
        assert_eq!(parse_spec("tcp/80:h:80").unwrap().kind, Kind::Tcp);
        assert_eq!(parse_spec("tls/8443:h:443").unwrap().kind, Kind::Tls);
        assert!(parse_spec("sctp/80:h:80").is_err());
    }

    #[test]
    fn tls_opts_become_wire_flags() {
        use blit_remote::net::{NET_OPEN_INSECURE, NET_OPEN_TLS};
        let plain = TlsOpts::default();
        assert_eq!(plain.flags(), NET_OPEN_TLS);
        assert!(!plain.insecure);
        assert!(plain.alpn.is_empty());
        let insecure = TlsOpts {
            insecure: true,
            ..TlsOpts::default()
        };
        assert_eq!(insecure.flags(), NET_OPEN_TLS | NET_OPEN_INSECURE);
        // The wire rejects INSECURE without TLS; the client can never construct that pair, which is why flags() always sets TLS.
        let open = NetOpen {
            flags: insecure.flags(),
            ..NetOpen::tcp(1, "h", 443)
        };
        assert_eq!(open.validate(), Ok(()));
    }

    #[test]
    fn explicit_bind_address() {
        let spec = parse_spec("0.0.0.0:8080:localhost:3000").unwrap();
        assert_eq!(spec.bind, "0.0.0.0");
        assert_eq!(spec.local_port, 8080);
        assert_eq!(spec.host, "localhost");
    }

    #[test]
    fn default_bind_is_loopback() {
        // The security property, asserted rather than assumed: an unauthenticated listener must not land on a wildcard address without the operator saying so.
        assert_eq!(parse_spec("8080:h:80").unwrap().bind, "127.0.0.1");
    }

    #[test]
    fn bracketed_ipv6_bind_and_host() {
        let spec = parse_spec("[::1]:8080:[fd00::5]:3000").unwrap();
        assert_eq!(spec.bind, "::1");
        assert_eq!(spec.host, "fd00::5");
        assert_eq!(spec.local_port, 8080);
        assert_eq!(spec.host_port, 3000);
    }

    #[test]
    fn ephemeral_local_port_is_allowed() {
        assert_eq!(parse_spec("0:db.internal:5432").unwrap().local_port, 0);
    }

    #[test]
    fn zero_target_port_is_rejected() {
        // The wire refuses port 0; catching it here gives a better message than a round trip to learn the same thing.
        assert!(parse_spec("8080:host:0").is_err());
    }

    #[test]
    fn malformed_specs_are_rejected() {
        for bad in [
            "8080",
            "8080:host",
            "",
            "8080:host:notaport",
            "notaport:host:80",
            "8080::80",
            "[::1:8080:host:80",
            "[::1]x:8080:host:80",
            "1:2:8080:host:80",
        ] {
            assert!(parse_spec(bad).is_err(), "{bad} parsed");
        }
    }

    #[test]
    fn display_round_trips() {
        for spec in [
            "8080:localhost:3000",
            "udp/5353:resolver.internal:53",
            "tls/8443:api.internal:443",
            "0.0.0.0:8080:localhost:3000",
        ] {
            let parsed = parse_spec(spec).unwrap();
            assert_eq!(parsed.to_string(), spec);
            assert_eq!(parse_spec(&parsed.to_string()).unwrap(), parsed);
        }
    }
}
