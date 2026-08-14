//! TCP and UDP relay (docs/design/net.md).
//! Connection-scoped sockets: the client names a host and port, the server opens a socket and copies payload.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, mpsc, watch};

use blit_remote::net::{
    C2S_NET_ACK, C2S_NET_CLOSE, C2S_NET_DATA, C2S_NET_DGRAM, C2S_NET_OPEN, NET_CLOSE_WRITE,
    NET_CLOSED_BUDGET, NET_CLOSED_EOF, NET_CLOSED_RESET, NET_CLOSED_TIMEOUT, NET_DGRAM_QUEUE,
    NET_MAX_CHUNK, NET_MAX_DGRAM, NET_MAX_SOCKETS, NET_STATUS_BUDGET, NET_STATUS_INVALID,
    NET_STATUS_NOT_FOUND, NET_STATUS_OK, NET_STATUS_PERMISSION, NET_STATUS_REFUSED, NET_STATUS_TLS,
    NET_WINDOW_AGGREGATE, NET_WINDOW_BYTES, NET_WINDOW_MIN, NetOpen, msg_net_ack_s2c,
    msg_net_closed, msg_net_data_s2c, msg_net_dgram_s2c, msg_net_opened, msg_net_opened_tcp,
    parse_net_ack_c2s, parse_net_close, parse_net_data_c2s, parse_net_dgram_c2s, parse_net_open,
};
use rustc_hash::FxHashMap;

/// Connect and TLS-handshake timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default UDP flow idle timeout.
const UDP_IDLE_DEFAULT: Duration = Duration::from_secs(60);

/// Outbox depth above which relayed datagrams are dropped rather than queued.
const OUTBOX_CONGESTED_BYTES: usize = 2 * 1024 * 1024;

/// How long a TCP reader waits before re-checking a congested outbox. A
/// datagram flow drops instead; a stream has to wait, and there is no
/// readiness signal to wait on.
const OUTBOX_BACKOFF: Duration = Duration::from_millis(5);

/// Per-flow queue byte cap, alongside the datagram count cap: 256 maximum sized datagrams would be 16 MiB, which is not a bound worth having.
const DGRAM_QUEUE_BYTES: usize = 1024 * 1024;

/// What the relay may reach, and whether it may skip TLS verification
/// (docs/design/net.md § Target policy).
///
/// **Unrestricted by default.** With no pattern the relay reaches whatever
/// the host reaches, which is the useful default for a server you run on
/// your own machines. Patterns turn it into an allowlist, for an operator
/// exposing a server to clients they do not fully trust — without them the
/// only control is `BLIT_NET=0`, which turns the family off entirely.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    insecure_allowed: bool,
    /// Empty = unrestricted, but only when `restricted` is false.
    allow: Vec<TargetRule>,
    /// An allowlist was asked for. Kept separate from `allow` being
    /// non-empty so that patterns which all fail to parse cannot widen the
    /// policy back to unrestricted — an operator who typed
    /// `--allow-forward` and mistyped it should get loopback, not the
    /// internet.
    restricted: bool,
}

impl Policy {
    /// `allow` are `host[:ports]` patterns; unparsable ones are reported and
    /// dropped, and patterns that all fail to parse leave loopback only
    /// rather than widening back to unrestricted.
    pub fn new(insecure_allowed: bool, allow: &[String]) -> Self {
        let env = std::env::var("BLIT_ALLOW_FORWARD").unwrap_or_default();
        let patterns = allow
            .iter()
            .map(String::as_str)
            .chain(env.split(',').filter(|p| !p.trim().is_empty()));
        let mut rules = Vec::new();
        let mut restricted = false;
        for pattern in patterns {
            restricted = true;
            match TargetRule::parse(pattern.trim()) {
                Some(rule) => rules.push(rule),
                None => eprintln!("blit: ignoring unparsable --allow-forward {pattern:?}"),
            }
        }
        if restricted && rules.is_empty() {
            eprintln!("blit: no --allow-forward pattern parsed; the relay reaches loopback only");
        }
        Self {
            insecure_allowed: insecure_allowed
                || std::env::var("BLIT_ALLOW_FORWARD_INSECURE").is_ok_and(|v| v == "1"),
            allow: rules,
            restricted,
        }
    }

    fn insecure_allowed(&self) -> bool {
        self.insecure_allowed
    }

    /// Whether the requested `host` may be reached on `port`.
    ///
    /// Checked against the requested *name* before resolution, so a name rule
    /// authorizes whatever that name resolves to — precisely the grant an
    /// operator writing `*.svc.internal` is asking for. Address and CIDR
    /// rules are checked again against the resolved addresses by
    /// [`Self::permits_addr`], which is what the connect actually uses: the
    /// gap between check and connect is where DNS rebinding lives, and the
    /// only way to close it is to check the address you are about to dial.
    fn permits_host(&self, host: &str, port: u16) -> bool {
        if !self.restricted {
            return true;
        }
        // Loopback always works, so a dev server does not need a rule
        // (docs/design/net.md § Target policy).
        if is_loopback_host(host) {
            return true;
        }
        self.allow.iter().any(|r| r.matches_host(host, port))
    }

    /// Whether a resolved address may be dialed on `port`. A name rule that
    /// already matched the requested host authorizes its addresses; address
    /// and CIDR rules are matched here.
    fn permits_addr(&self, host: &str, addr: SocketAddr) -> bool {
        if !self.restricted || addr.ip().is_loopback() || is_loopback_host(host) {
            return true;
        }
        self.allow
            .iter()
            .any(|r| r.matches_addr(host, addr.ip(), addr.port()))
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// One `--allow-forward` pattern: `host[:ports]`.
#[derive(Clone, Debug)]
struct TargetRule {
    host: HostRule,
    /// Empty = every port.
    ports: Vec<(u16, u16)>,
}

#[derive(Clone, Debug)]
enum HostRule {
    Any,
    /// `*.suffix` — matches the suffix itself and anything under it.
    Suffix(String),
    Exact(String),
    Addr(std::net::IpAddr),
    /// An address and a prefix length.
    Cidr(std::net::IpAddr, u8),
}

impl TargetRule {
    fn parse(pattern: &str) -> Option<Self> {
        if pattern.is_empty() {
            return None;
        }
        // Split host from ports on the *last* colon, but only when what
        // follows looks like a port list — otherwise `::1` and `2001:db8::/32`
        // would lose their tails.
        let (host_part, port_part) = match pattern.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.starts_with(|c: char| c.is_ascii_digit()) => {
                // A bare IPv6 literal ends in digits too; brackets or a
                // remaining colon in the host tell them apart.
                if h.ends_with(']') || !h.contains(':') {
                    (h, Some(p))
                } else {
                    (pattern, None)
                }
            }
            _ => (pattern, None),
        };
        let host_part = host_part.trim_start_matches('[').trim_end_matches(']');
        let host = if host_part == "*" {
            HostRule::Any
        } else if let Some(suffix) = host_part.strip_prefix("*.") {
            if suffix.is_empty() {
                return None;
            }
            HostRule::Suffix(suffix.to_ascii_lowercase())
        } else if let Some((addr, bits)) = host_part.split_once('/') {
            let ip: std::net::IpAddr = addr.parse().ok()?;
            let bits: u8 = bits.parse().ok()?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            if bits > max {
                return None;
            }
            HostRule::Cidr(ip, bits)
        } else if let Ok(ip) = host_part.parse::<std::net::IpAddr>() {
            HostRule::Addr(ip)
        } else if host_part.is_empty() || host_part.contains(':') {
            // A leftover colon means the port list did not parse as one, and
            // no hostname contains a colon: `host:notaport` is a typo, not a
            // machine named "host:notaport".
            return None;
        } else {
            HostRule::Exact(host_part.to_ascii_lowercase())
        };

        let mut ports = Vec::new();
        if let Some(list) = port_part {
            for item in list.split(',') {
                let item = item.trim();
                let (lo, hi) = match item.split_once('-') {
                    Some((lo, hi)) => (lo.parse::<u16>().ok()?, hi.parse::<u16>().ok()?),
                    None => {
                        let p = item.parse::<u16>().ok()?;
                        (p, p)
                    }
                };
                if lo > hi {
                    return None;
                }
                ports.push((lo, hi));
            }
            if ports.is_empty() {
                return None;
            }
        }
        Some(Self { host, ports })
    }

    fn port_ok(&self, port: u16) -> bool {
        self.ports.is_empty() || self.ports.iter().any(|(lo, hi)| port >= *lo && port <= *hi)
    }

    fn matches_host(&self, host: &str, port: u16) -> bool {
        if !self.port_ok(port) {
            return false;
        }
        let lower = host.to_ascii_lowercase();
        match &self.host {
            HostRule::Any => true,
            HostRule::Suffix(suffix) => lower == *suffix || lower.ends_with(&format!(".{suffix}")),
            HostRule::Exact(name) => lower == *name,
            // An address rule matches a requested literal directly, and
            // otherwise waits for resolution.
            HostRule::Addr(ip) => lower.parse::<std::net::IpAddr>().is_ok_and(|h| h == *ip),
            HostRule::Cidr(net, bits) => lower
                .parse::<std::net::IpAddr>()
                .is_ok_and(|h| in_cidr(h, *net, *bits)),
        }
    }

    fn matches_addr(&self, host: &str, ip: std::net::IpAddr, port: u16) -> bool {
        if !self.port_ok(port) {
            return false;
        }
        match &self.host {
            HostRule::Any => true,
            HostRule::Addr(want) => ip == *want,
            HostRule::Cidr(net, bits) => in_cidr(ip, *net, *bits),
            // A name rule authorizes what that name resolves to; it already
            // matched the requested host or we would not be here.
            HostRule::Suffix(_) | HostRule::Exact(_) => self.matches_host(host, port),
        }
    }
}

/// Whether `ip` falls in `net/bits`. Mixed families never match: a v4 rule
/// does not silently cover a v4-mapped v6 address, which would be a way to
/// slip past an allowlist.
fn in_cidr(ip: std::net::IpAddr, net: std::net::IpAddr, bits: u8) -> bool {
    fn masked(bytes: &[u8], bits: u8) -> Vec<u8> {
        let mut out = bytes.to_vec();
        let full = (bits / 8) as usize;
        for (i, b) in out.iter_mut().enumerate() {
            if i < full {
                continue;
            }
            if i == full {
                let rest = bits % 8;
                *b &= if rest == 0 { 0 } else { !0u8 << (8 - rest) };
            } else {
                *b = 0;
            }
        }
        out
    }
    match (ip, net) {
        (std::net::IpAddr::V4(a), std::net::IpAddr::V4(b)) => {
            masked(&a.octets(), bits) == masked(&b.octets(), bits)
        }
        (std::net::IpAddr::V6(a), std::net::IpAddr::V6(b)) => {
            masked(&a.octets(), bits) == masked(&b.octets(), bits)
        }
        _ => false,
    }
}

// --------------------------------------------------------------------------- Datagram queue ---------------------------------------------------------------------------

/// A bounded datagram queue that drops the **oldest** when full: for nearly every UDP protocol the newest datagram is the useful one, and a stale queue is latency with no payoff (docs/design/net.md § UDP flows).
struct DgramQueue {
    inner: Mutex<VecDeque<Vec<u8>>>,
    bytes: AtomicU64,
    dropped: AtomicU64,
    notify: Notify,
    closed: AtomicU64,
}

impl DgramQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            notify: Notify::new(),
            closed: AtomicU64::new(0),
        }
    }

    async fn push(&self, payload: Vec<u8>) {
        let mut q = self.inner.lock().await;
        let mut bytes = self.bytes.load(Ordering::Relaxed) as usize;
        while q.len() >= NET_DGRAM_QUEUE || bytes + payload.len() > DGRAM_QUEUE_BYTES {
            match q.pop_front() {
                Some(old) => {
                    bytes = bytes.saturating_sub(old.len());
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                None => break,
            }
        }
        bytes += payload.len();
        self.bytes.store(bytes as u64, Ordering::Relaxed);
        q.push_back(payload);
        drop(q);
        self.notify.notify_one();
    }

    /// Next datagram, or `None` once the queue is closed and drained.
    async fn pop(&self) -> Option<Vec<u8>> {
        loop {
            {
                let mut q = self.inner.lock().await;
                if let Some(payload) = q.pop_front() {
                    self.bytes
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                            Some(v.saturating_sub(payload.len() as u64))
                        })
                        .ok();
                    return Some(payload);
                }
                if self.closed.load(Ordering::Relaxed) != 0 {
                    return None;
                }
            }
            self.notify.notified().await;
        }
    }

    fn close(&self) {
        self.closed.store(1, Ordering::Relaxed);
        self.notify.notify_one();
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

// --------------------------------------------------------------------------- Connection-scoped socket table ---------------------------------------------------------------------------

/// One relayed socket's handles, held by the connection task.
struct Entry {
    /// Set by the socket task as it exits. The connection task owns the map, so
    /// a finished socket cannot remove itself — without this its slot is held
    /// forever and the budget fills with corpses.
    done: Arc<std::sync::atomic::AtomicBool>,
    /// Bytes the client has written; `None` for a UDP flow.
    ack: Option<watch::Sender<u64>>,
    /// TCP: bytes to write, and write-side shutdown.
    data: DataSink,
    /// TCP: the client→target window; `None` for a UDP flow, which drops.
    inbound: Option<Arc<InboundWindow>>,
    /// Dropped to abort the socket task.
    abort: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Entry {
    #[cfg(test)]
    fn is_udp(&self) -> bool {
        matches!(self.data, DataSink::Udp(_))
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

enum DataSink {
    Tcp(mpsc::UnboundedSender<TcpWrite>),
    Udp(Arc<DgramQueue>),
}

enum TcpWrite {
    Data(Vec<u8>),
    ShutdownWrite,
}

/// Every relayed socket on one blit connection.
#[derive(Default)]
pub struct NetSockets {
    map: FxHashMap<u16, Entry>,
    /// Outbox byte counter shared with the sender loop, for the advisory congestion check that paces relayed datagrams.
    outbox_bytes: Option<Arc<std::sync::atomic::AtomicUsize>>,
    outbox_frames: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Bytes sent to the client and not yet acked, summed over every TCP
    /// stream here. The per-stream window bounds one stream; this bounds the
    /// connection (docs/design/net.md § Pacing), which per-stream shares
    /// alone cannot: each is computed at open from the socket count of that
    /// moment and floored at two chunks, so with many streams their sum runs
    /// well past the aggregate.
    outstanding: Arc<std::sync::atomic::AtomicU64>,
    /// Woken when any stream's ack advances, so a reader parked on the
    /// aggregate re-checks instead of polling for credit that is already
    /// there.
    credit: Arc<tokio::sync::Notify>,
    /// The client→target counterpart of `outstanding`: bytes accepted from the
    /// client and not yet written to a target, summed over every stream here.
    inbound_total: Arc<std::sync::atomic::AtomicU64>,
}

impl NetSockets {
    pub fn ids(&self) -> impl Iterator<Item = u16> + '_ {
        self.map.keys().copied()
    }

    pub fn with_outbox(
        frames: Arc<std::sync::atomic::AtomicUsize>,
        bytes: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            map: FxHashMap::default(),
            outbox_bytes: Some(bytes),
            outbox_frames: Some(frames),
            ..Self::default()
        }
    }

    fn counters(&self) -> Option<OutboxCounters> {
        Some(OutboxCounters {
            frames: self.outbox_frames.clone()?,
            bytes: self.outbox_bytes.clone()?,
        })
    }
}

/// The pair of shared outbox counters, cloned into socket tasks so their sends are accounted the same way session sends are.
#[derive(Clone)]
struct OutboxCounters {
    frames: Arc<std::sync::atomic::AtomicUsize>,
    bytes: Arc<std::sync::atomic::AtomicUsize>,
}

impl OutboxCounters {
    fn queued_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    fn on_send(&self, len: usize) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(len, Ordering::Relaxed);
    }
}

/// Send one message to the client, accounting it in the shared outbox counters so the congestion check has something honest to read.
fn emit(out: &mpsc::UnboundedSender<Vec<u8>>, counters: &Option<OutboxCounters>, msg: Vec<u8>) {
    let len = msg.len();
    if out.send(msg).is_ok()
        && let Some(c) = counters
    {
        c.on_send(len);
    }
}

// --------------------------------------------------------------------------- Dispatch ---------------------------------------------------------------------------

/// Handle one `NET_*` message.
pub async fn handle_net_message(
    data: &[u8],
    sockets: &mut NetSockets,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    policy: &Policy,
    verbose: bool,
) {
    match data[0] {
        C2S_NET_OPEN => {
            let Some(open) = parse_net_open(data) else {
                // Every NET_OPEN gets exactly one reply. The stream id sits
                // at a fixed offset, so a message that carries one can still
                // be refused by name — which is the difference between a
                // client learning its request was malformed and a client
                // waiting in OPENING forever. `TLS` set with the TLS block
                // absent is the case that reaches here in practice, and
                // net.md lists it as INVALID.
                match data.get(1..3) {
                    Some(bytes) => {
                        let id = u16::from_le_bytes([bytes[0], bytes[1]]);
                        emit(
                            out,
                            &sockets.counters(),
                            msg_net_opened(id, NET_STATUS_INVALID, "", "malformed NET_OPEN"),
                        );
                    }
                    // Too short to carry an id: nothing to name, and a client
                    // this broken has no state to correct.
                    None => {
                        if verbose {
                            eprintln!("[net] malformed NET_OPEN");
                        }
                    }
                }
                return;
            };
            let id = open.stream_id;
            let counters = sockets.counters();
            let refuse = |status: u8, detail: &str| {
                emit(out, &counters, msg_net_opened(id, status, "", detail));
            };
            if let Err(detail) = open.validate() {
                refuse(NET_STATUS_INVALID, detail);
                return;
            }
            // Sockets whose task has exited still occupy the map until now:
            // the task cannot remove itself (the connection task owns the
            // map). Sweep before *both* checks below. Sweeping only before
            // the budget check meant a stream the server closed — a target
            // EOF or reset, which never removes the entry — left a corpse
            // that failed the liveness test, so the documented right to
            // reuse an id after NET_CLOSED did not exist for any close the
            // client did not itself initiate.
            sockets.map.retain(|_, entry| !entry.is_done());
            if sockets.map.contains_key(&id) {
                refuse(NET_STATUS_INVALID, "stream id already live");
                return;
            }
            if sockets.map.len() >= NET_MAX_SOCKETS {
                refuse(NET_STATUS_BUDGET, "too many open sockets");
                return;
            }
            if open.flags & blit_remote::net::NET_OPEN_INSECURE != 0 && !policy.insecure_allowed() {
                // Refused rather than silently downgraded to verifying: a client that asked to skip verification and got it anyway would be told its stream is checked when it is not, and the reverse would fail confusingly on a self-signed cert.
                refuse(
                    NET_STATUS_PERMISSION,
                    "certificate verification may not be skipped (see --allow-forward-insecure)",
                );
                return;
            }
            open_socket(open, sockets, out, policy, verbose);
        }
        C2S_NET_DATA => {
            let Some((id, payload)) = parse_net_data_c2s(data) else {
                return;
            };
            if payload.len() > NET_MAX_CHUNK {
                close_with(sockets, out, id, NET_CLOSED_BUDGET, "chunk too large");
                return;
            }
            let Some(entry) = sockets.map.get(&id) else {
                return;
            };
            match &entry.data {
                DataSink::Tcp(tx) => {
                    // Refused, not awaited: this runs on the dispatch loop, so
                    // waiting for the target to drain would stall every other
                    // stream and the client's keystrokes with them.
                    if !entry
                        .inbound
                        .as_ref()
                        .is_some_and(|w| w.admit(payload.len()))
                    {
                        close_with(
                            sockets,
                            out,
                            id,
                            NET_CLOSED_BUDGET,
                            "unacked client data exceeded the window",
                        );
                        return;
                    }
                    let _ = tx.send(TcpWrite::Data(payload.to_vec()));
                }
                // A stream write on a datagram flow: INVALID per the wire, and fatal to the socket rather than silently ignored, because a client confusing the two will keep doing it.
                DataSink::Udp(_) => close_with(
                    sockets,
                    out,
                    id,
                    NET_CLOSED_POLICY_INVALID,
                    "NET_DATA on a UDP flow",
                ),
            }
        }
        C2S_NET_DGRAM => {
            let Some((id, payload)) = parse_net_dgram_c2s(data) else {
                return;
            };
            let Some(entry) = sockets.map.get(&id) else {
                return;
            };
            match &entry.data {
                DataSink::Udp(queue) => {
                    if payload.len() > NET_MAX_DGRAM {
                        // Dropped and counted, not truncated and not an error: a truncated datagram is a corrupted one.
                        queue.dropped.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    queue.push(payload.to_vec()).await;
                }
                DataSink::Tcp(_) => close_with(
                    sockets,
                    out,
                    id,
                    NET_CLOSED_POLICY_INVALID,
                    "NET_DGRAM on a TCP stream",
                ),
            }
        }
        C2S_NET_ACK => {
            let Some((id, bytes)) = parse_net_ack_c2s(data) else {
                return;
            };
            if let Some(entry) = sockets.map.get(&id)
                && let Some(ack) = &entry.ack
            {
                // A stale or duplicate ack is ignored; watch keeps the highest value the reader has seen.
                let advanced = ack.send_if_modified(|current| {
                    if bytes > *current {
                        *current = bytes;
                        true
                    } else {
                        false
                    }
                });
                // The reader clamps to what it actually sent, so an ack for
                // bytes that do not exist yet buys nothing; waking on it is
                // harmless and the check belongs where `sent` is known.
                if advanced {
                    sockets.credit.notify_waiters();
                }
            }
        }
        C2S_NET_CLOSE => {
            let Some((id, flags)) = parse_net_close(data) else {
                return;
            };
            let Some(entry) = sockets.map.get(&id) else {
                return;
            };
            if flags & NET_CLOSE_WRITE != 0 {
                match &entry.data {
                    DataSink::Tcp(tx) => {
                        let _ = tx.send(TcpWrite::ShutdownWrite);
                    }
                    // A datagram flow has no write side to shut down.
                    DataSink::Udp(_) => close_with(
                        sockets,
                        out,
                        id,
                        NET_CLOSED_POLICY_INVALID,
                        "half-close on a UDP flow",
                    ),
                }
                return;
            }
            // Full close. The task's abort path returns without emitting, so
            // the reply and the slot are both this handler's job — otherwise
            // the client waits for a NET_CLOSED that never comes and the entry
            // holds a budget slot forever.
            if let Some(entry) = sockets.map.remove(&id) {
                if let DataSink::Udp(q) = &entry.data {
                    q.close();
                }
                drop(entry.abort);
                emit(
                    out,
                    &sockets.counters(),
                    msg_net_closed(id, NET_CLOSED_EOF, ""),
                );
            }
        }
        _ => {}
    }
}

/// `NET_CLOSED` reason for a client that broke the wire contract.
const NET_CLOSED_POLICY_INVALID: u8 = blit_remote::net::NET_CLOSED_POLICY;

fn close_with(
    sockets: &mut NetSockets,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    id: u16,
    reason: u8,
    detail: &str,
) {
    let counters = sockets.counters();
    if sockets.map.remove(&id).is_some() {
        emit(out, &counters, msg_net_closed(id, reason, detail));
    }
}

/// Refuse every `NET_OPEN` when the family is disabled, so a client that ignores feature bits still gets its one reply rather than waiting forever.
pub fn refuse_net_message(data: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
    if data[0] == C2S_NET_OPEN && data.len() >= 3 {
        let id = u16::from_le_bytes([data[1], data[2]]);
        let _ = out.send(msg_net_opened(
            id,
            NET_STATUS_PERMISSION,
            "",
            "relay disabled on this server",
        ));
    }
}

/// Tear down every socket on connection loss, so a disconnect releases the host's sockets rather than leaking them until the process exits.
pub fn shutdown(sockets: &mut NetSockets) {
    for (_, entry) in sockets.map.drain() {
        if let DataSink::Udp(q) = &entry.data {
            q.close();
        }
        drop(entry.abort);
    }
}

// --------------------------------------------------------------------------- Opening ---------------------------------------------------------------------------

/// What every socket task needs from the connection it belongs to.
struct StreamCtx {
    out: mpsc::UnboundedSender<Vec<u8>>,
    counters: Option<OutboxCounters>,
    /// Set as the task exits: the connection task owns the map, so a
    /// finished socket cannot remove its own entry.
    done: Arc<std::sync::atomic::AtomicBool>,
    abort: tokio::sync::oneshot::Receiver<()>,
    verbose: bool,
}

/// The client-facing halves of a TCP stream, made before the target is
/// reached.
struct TcpCtx {
    inbound: Arc<InboundWindow>,
    write_rx: mpsc::UnboundedReceiver<TcpWrite>,
    ack_rx: watch::Receiver<u64>,
    window: u64,
    outstanding: Arc<std::sync::atomic::AtomicU64>,
    credit: Arc<tokio::sync::Notify>,
}

/// Accept a `NET_OPEN`: create the client-facing halves, put the entry in
/// the map, and leave reaching the target to a task.
///
/// Nothing here awaits. This runs on the per-connection dispatch loop —
/// the same loop that reads `C2S_INPUT` — so a DNS lookup, a connect walk
/// over every resolved address, and a TLS handshake used to run *between*
/// two keystrokes: one `NET_OPEN` naming a slow or unreachable multi-A
/// host froze that client's terminal, and every other stream on it, for up
/// to N×10 s + 10 s. It also defeated the point of ranking relay data
/// below the focused PTY, since the loop never got as far as scheduling.
///
/// Reserving the entry up front has a second effect worth having: data the
/// client pipelines behind its `NET_OPEN` now waits in the stream's own
/// channel until the target is up, where before it arrived to find no map
/// entry and was dropped.
fn open_socket(
    open: NetOpen,
    sockets: &mut NetSockets,
    out: &mpsc::UnboundedSender<Vec<u8>>,
    policy: &Policy,
    verbose: bool,
) {
    let id = open.stream_id;
    // The name check happens here, synchronously: refusing a target is not
    // worth a task, and an operator reading the log wants the refusal in the
    // order the client asked for it.
    if !policy.permits_host(&open.host, open.port) {
        emit(
            out,
            &sockets.counters(),
            msg_net_opened(
                id,
                NET_STATUS_PERMISSION,
                "",
                "target not permitted (see --allow-forward)",
            ),
        );
        return;
    }
    let policy = policy.clone();
    let (abort_tx, abort) = tokio::sync::oneshot::channel::<()>();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ctx = StreamCtx {
        out: out.clone(),
        counters: sockets.counters(),
        done: done.clone(),
        abort,
        verbose,
    };

    if open.is_udp() {
        let queue = Arc::new(DgramQueue::new());
        sockets.map.insert(
            id,
            Entry {
                done,
                ack: None,
                data: DataSink::Udp(queue.clone()),
                inbound: None,
                abort: Some(abort_tx),
            },
        );
        tokio::spawn(run_udp(open, ctx, policy, queue));
    } else {
        let (write_tx, write_rx) = mpsc::unbounded_channel::<TcpWrite>();
        let (ack_tx, ack_rx) = watch::channel(0u64);
        // The entry counts against the socket budget from now, so the share
        // includes this stream.
        let window = per_stream_window(sockets.map.len() + 1);
        // The same window bounds both directions.
        let inbound = Arc::new(InboundWindow {
            queued: std::sync::atomic::AtomicU64::new(0),
            limit: window,
            total: sockets.inbound_total.clone(),
        });
        let tcp = TcpCtx {
            inbound: inbound.clone(),
            write_rx,
            ack_rx,
            window,
            outstanding: sockets.outstanding.clone(),
            credit: sockets.credit.clone(),
        };
        sockets.map.insert(
            id,
            Entry {
                done,
                ack: Some(ack_tx),
                data: DataSink::Tcp(write_tx),
                inbound: Some(inbound),
                abort: Some(abort_tx),
            },
        );
        tokio::spawn(run_tcp(open, ctx, policy, tcp));
    }
}

/// Resolve `host:port`, or the status and detail to refuse the open with.
async fn resolve(open: &NetOpen) -> Result<Vec<SocketAddr>, (u8, String)> {
    let target = format!("{}:{}", open.host, open.port);
    let resolved =
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host(&target)).await {
            Ok(Ok(addrs)) => addrs.collect::<Vec<SocketAddr>>(),
            Ok(Err(e)) => return Err((NET_STATUS_NOT_FOUND, e.to_string())),
            Err(_) => return Err((NET_STATUS_NOT_FOUND, "resolution timed out".into())),
        };
    if resolved.is_empty() {
        return Err((NET_STATUS_NOT_FOUND, "no addresses".into()));
    }
    Ok(resolved)
}

async fn run_tcp(open: NetOpen, ctx: StreamCtx, policy: Policy, tcp: TcpCtx) {
    let id = open.stream_id;
    let TcpCtx {
        inbound,
        mut write_rx,
        mut ack_rx,
        window,
        outstanding,
        credit,
    } = tcp;
    let StreamCtx {
        out,
        counters,
        done,
        abort: mut abort_rx,
        verbose,
    } = ctx;
    let refuse = |status: u8, detail: &str| {
        emit(&out, &counters, msg_net_opened(id, status, "", detail));
        done.store(true, Ordering::Relaxed);
    };
    // Cancellable at every step: a connection that drops mid-connect should
    // release the target socket now, not when the timeout expires.
    let reached = tokio::select! {
        r = async {
            let resolved = match resolve(&open).await {
                Ok(r) => r,
                Err((status, detail)) => return Err(Some((status, detail))),
            };
            let mut last_err = String::from("no route");
            let mut refused_by_policy = false;
            for addr in &resolved {
                // Check the address we are about to dial, never re-resolving
                // between the two: that gap is the DNS-rebinding hole, and
                // this is the only place it can be closed.
                if !policy.permits_addr(&open.host, *addr) {
                    refused_by_policy = true;
                    continue;
                }
                match tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr))
                    .await
                {
                    Ok(Ok(s)) => return Ok(s),
                    Ok(Err(e)) => last_err = e.to_string(),
                    Err(_) => last_err = "connect timed out".into(),
                }
            }
            if refused_by_policy {
                return Err(Some((
                    NET_STATUS_PERMISSION,
                    "resolved address not permitted (see --allow-forward)".into(),
                )));
            }
            Err(Some((NET_STATUS_REFUSED, last_err)))
        } => r,
        _ = &mut abort_rx => Err(None),
    };
    let stream = match reached {
        Ok(s) => s,
        Err(Some((status, detail))) => return refuse(status, &detail),
        // Aborted: the client is gone, so there is nobody to tell.
        Err(None) => return done.store(true, Ordering::Relaxed),
    };
    // Every consumer of a relayed interactive stream wants this, and one that does not can batch its own writes.
    let _ = stream.set_nodelay(true);

    // Optional TLS termination: from here down the relay carries plaintext either way, which is the whole point for a client that cannot terminate for itself (docs/design/net.md § TLS termination).
    let (reader, writer, alpn) = if open.is_tls() {
        let shaken = tokio::select! {
            r = handshake(stream, &open) => r,
            _ = &mut abort_rx => return done.store(true, Ordering::Relaxed),
        };
        match shaken {
            Ok((tls, alpn)) => {
                let (r, w) = tokio::io::split(tls);
                (
                    Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                    Box::new(w) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
                    alpn,
                )
            }
            Err(detail) => return refuse(NET_STATUS_TLS, &detail),
        }
    } else {
        let (r, w) = stream.into_split();
        (
            Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
            Box::new(w) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
            String::new(),
        )
    };

    // The window rides the accept: the client is pipelining data behind its
    // open already, and until this arrives it can only assume the smallest
    // share the server ever grants (docs/design/net.md § Pacing).
    emit(&out, &counters, msg_net_opened_tcp(id, &alpn, window));
    if verbose {
        let how = if open.is_tls() {
            if alpn.is_empty() {
                " (tls)".to_string()
            } else {
                format!(" (tls, alpn={alpn})")
            }
        } else {
            String::new()
        };
        eprintln!("[net] stream {id} → {}:{}{how}", open.host, open.port);
    }

    {
        let (mut reader, mut writer) = (reader, writer);
        // Client → target.
        let write_out = out.clone();
        let write_counters = counters.clone();
        let write_inbound = inbound.clone();
        let writer_task = tokio::spawn(async move {
            let mut written: u64 = 0;
            while let Some(cmd) = write_rx.recv().await {
                match cmd {
                    TcpWrite::Data(bytes) => {
                        let len = bytes.len();
                        let ok = writer.write_all(&bytes).await.is_ok();
                        // Released whether or not the write landed: a failed
                        // write ends the stream, and holding the charge would
                        // only matter if it did not.
                        write_inbound.release(len);
                        if !ok {
                            break;
                        }
                        written += len as u64;
                        emit(&write_out, &write_counters, msg_net_ack_s2c(id, written));
                    }
                    TcpWrite::ShutdownWrite => {
                        let _ = writer.shutdown().await;
                        break;
                    }
                }
            }
        });

        // Target → client, paced by the client's cumulative ack.
        let mut buf = vec![0u8; NET_MAX_CHUNK];
        let mut sent: u64 = 0;
        // This stream's share of the connection-wide outstanding total, kept
        // in step with `sent - acked` so the aggregate is the sum of what
        // every stream is actually waiting on.
        let mut charge = CreditCharge::new(outstanding, credit.clone());
        let (reason, detail) = loop {
            let mut forged = false;
            loop {
                let acked = *ack_rx.borrow_and_update();
                // An ack cannot name bytes that were never sent. Normalizing
                // it would be worse than useless: `sent - ack` saturated to
                // zero, so `u64::MAX` bought infinite credit and a client
                // that acked it without draining its socket pulled the
                // target's whole stream into the server's outbox. It is a
                // forgery, and a forgery ends the stream.
                if acked > sent {
                    forged = true;
                    break;
                }
                let mine = sent - acked;
                charge.set(mine);
                // Both gates must open: this stream's own credit, and the
                // connection's aggregate. Neither subsumes the other — one
                // stream can exhaust its window while the connection is
                // idle, and many streams can each stay under a window while
                // together exceeding the aggregate.
                if mine < window && charge.total.load(Ordering::Relaxed) < NET_WINDOW_AGGREGATE {
                    break;
                }
                // Register before re-reading, so an ack landing between the
                // check and the wait is not missed.
                let waiting = credit.notified();
                tokio::select! {
                    changed = ack_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    _ = waiting => {}
                    _ = &mut abort_rx => return,
                }
            }
            if forged {
                break (
                    NET_CLOSED_POLICY_INVALID,
                    "NET_ACK ahead of bytes sent".to_string(),
                );
            }
            // An honest client may legitimately ack everything the instant
            // it arrives, which leaves the window permanently open — so the
            // window is not what bounds server memory. The outbox is: it is
            // unbounded by construction (the writer must never deadlock), so
            // the reader stops pulling from the target while the connection
            // is behind on what it has already queued.
            while counters
                .as_ref()
                .is_some_and(|c| c.queued_bytes() > OUTBOX_CONGESTED_BYTES)
            {
                // Nothing to wait on — the outbox drains on the writer's
                // schedule — so this polls. Coarse on purpose: it only runs
                // when the connection is already behind, where a few
                // milliseconds of latency is not the problem.
                tokio::select! {
                    _ = tokio::time::sleep(OUTBOX_BACKOFF) => {}
                    _ = &mut abort_rx => return,
                }
            }
            let read = tokio::select! {
                r = reader.read(&mut buf) => r,
                _ = &mut abort_rx => return,
            };
            match read {
                Ok(0) => break (NET_CLOSED_EOF, String::new()),
                Ok(n) => {
                    sent += n as u64;
                    emit(&out, &counters, msg_net_data_s2c(id, &buf[..n]));
                }
                // A TLS peer that closes without `close_notify` is an `UnexpectedEof`, and reporting that as a reset would be wrong twice over: the payload is complete as far as this relay can tell, and a great many real servers simply do not send the alert.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    if verbose {
                        eprintln!("[net] stream {id} ended without TLS close_notify");
                    }
                    break (NET_CLOSED_EOF, String::new());
                }
                Err(e) => break (NET_CLOSED_RESET, e.to_string()),
            }
        };
        writer_task.abort();
        emit(&out, &counters, msg_net_closed(id, reason, &detail));
        done.store(true, Ordering::Relaxed);
    }
}

async fn run_udp(open: NetOpen, ctx: StreamCtx, policy: Policy, queue: Arc<DgramQueue>) {
    let id = open.stream_id;
    let StreamCtx {
        out,
        counters,
        done,
        abort: mut abort_rx,
        verbose,
    } = ctx;
    let refuse = |status: u8, detail: &str| {
        emit(&out, &counters, msg_net_opened(id, status, "", detail));
        done.store(true, Ordering::Relaxed);
    };
    let resolved = tokio::select! {
        r = resolve(&open) => r,
        _ = &mut abort_rx => return done.store(true, Ordering::Relaxed),
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err((status, detail)) => return refuse(status, &detail),
    };
    let addr = resolved[0];
    if !policy.permits_addr(&open.host, addr) {
        return refuse(
            NET_STATUS_PERMISSION,
            "resolved address not permitted (see --allow-forward)",
        );
    }
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = match tokio::net::UdpSocket::bind(bind).await {
        Ok(s) => s,
        Err(e) => return refuse(NET_STATUS_OTHER_LOCAL, &e.to_string()),
    };
    // Connect the socket: it can only send to the target and only receives from it, so a flow can never be aimed at a third party mid-life — the property that keeps this from being a reflector.
    if let Err(e) = socket.connect(addr).await {
        return refuse(NET_STATUS_REFUSED, &e.to_string());
    }
    emit(&out, &counters, msg_net_opened(id, NET_STATUS_OK, "", ""));
    if verbose {
        eprintln!("[net] flow {id} → {}:{} (udp)", open.host, open.port);
    }

    let idle = udp_idle_timeout();
    {
        let mut buf = vec![0u8; NET_MAX_DGRAM];
        let mut down_dropped: u64 = 0;
        let (reason, detail) = loop {
            tokio::select! {
                _ = &mut abort_rx => {
                    // Client closed the flow; it still gets a NET_CLOSED.
                    break (NET_CLOSED_EOF, drops_detail(queue.dropped(), down_dropped));
                }
                outgoing = queue.pop() => {
                    match outgoing {
                        Some(payload) => {
                            if socket.send(&payload).await.is_err() {
                                break (NET_CLOSED_RESET, drops_detail(queue.dropped(), down_dropped));
                            }
                        }
                        None => break (NET_CLOSED_EOF, drops_detail(queue.dropped(), down_dropped)),
                    }
                }
                incoming = tokio::time::timeout(idle, socket.recv(&mut buf)) => {
                    match incoming {
                        Ok(Ok(n)) => {
                            // Drop rather than queue when the connection is congested: the only honest response to a full path on a datagram relay is to discard.
                            let congested = counters
                                .as_ref()
                                .is_some_and(|c| c.queued_bytes() > OUTBOX_CONGESTED_BYTES);
                            if congested {
                                down_dropped += 1;
                            } else {
                                emit(&out, &counters, msg_net_dgram_s2c(id, &buf[..n]));
                            }
                        }
                        // A connected UDP socket surfaces ICMP port-unreachable here; swallowing it is what makes a misconfigured forward look like a hung one.
                        Ok(Err(e)) => break (NET_CLOSED_RESET, e.to_string()),
                        Err(_) => break (
                            NET_CLOSED_TIMEOUT,
                            drops_detail(queue.dropped(), down_dropped),
                        ),
                    }
                }
            }
        };
        emit(&out, &counters, msg_net_closed(id, reason, &detail));
        done.store(true, Ordering::Relaxed);
    }
}

/// Status for a local failure with no better code; `OTHER` carries its diagnostic in `detail`.
const NET_STATUS_OTHER_LOCAL: u8 = blit_remote::net::NET_STATUS_OTHER;

// --------------------------------------------------------------------------- TLS termination ---------------------------------------------------------------------------

/// Handshake timeout, separate from the connect timeout it follows: a target that accepts and then stalls must fail with a status rather than hang.
const TLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Terminate TLS toward the target, returning the stream and the negotiated ALPN protocol (empty when none was offered or agreed).
async fn handshake(
    stream: tokio::net::TcpStream,
    open: &NetOpen,
) -> Result<
    (
        tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
        String,
    ),
    String,
> {
    let server_name = rustls::pki_types::ServerName::try_from(open.effective_sni().to_string())
        .map_err(|_| format!("not a valid server name: {}", open.effective_sni()))?;
    let config = client_config(open)?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tls = tokio::time::timeout(TLS_TIMEOUT, connector.connect(server_name, stream))
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let alpn = tls
        .get_ref()
        .1
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .unwrap_or_default();
    Ok((tls, alpn))
}

/// Build the client config for one open.
fn client_config(open: &NetOpen) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS config: {e}"))?;
    let mut config = if open.flags & blit_remote::net::NET_OPEN_INSECURE != 0 {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify { provider }))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        let loaded = rustls_native_certs::load_native_certs();
        for cert in loaded.certs {
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            // Verifying against an empty root store would fail every handshake with a confusing "unknown issuer"; say the real thing instead.
            return Err("no system trust roots available".into());
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    // ALPN is what the client asked to offer, verbatim and in order.
    config.alpn_protocols = open
        .alpn
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .filter(|p| !p.is_empty())
        .collect();
    Ok(config)
}

/// Accepts any certificate, for `NET_OPEN_INSECURE`.
#[derive(Debug)]
struct NoVerify {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Drop counts are the one thing about a relayed flow a user cannot infer from outside, so they ride the close (docs/design/net.md § Statuses).
fn drops_detail(up: u64, down: u64) -> String {
    if up == 0 && down == 0 {
        String::new()
    } else {
        format!("dropped {up} up, {down} down")
    }
}

/// Bytes a client has sent toward the target that are not yet written.
///
/// The client→target direction has a window too (docs/design/net.md § Pacing):
/// the server acks with `NET_ACK` after each write, and the client is supposed
/// to stop at the window. Supposed to is not enough — the queue feeding the
/// target is unbounded so the writer can never deadlock, so a client that
/// ignores its window, or that pipelines `NET_DATA` behind an open to a
/// deliberately slow host (which no longer blocks), grows server memory
/// without limit. This is the mirror of the forged-ack case in the other
/// direction, and it gets the same answer: exceeding the window is a broken
/// client, and the stream ends rather than the server absorbing it.
struct InboundWindow {
    queued: std::sync::atomic::AtomicU64,
    limit: u64,
    /// Client→target bytes buffered across every stream on this connection.
    /// Per-stream limits alone do not bound it: they floor at two chunks, so
    /// enough streams and their limits sum well past any single figure.
    total: Arc<std::sync::atomic::AtomicU64>,
}

impl InboundWindow {
    /// Charge `len` bytes against this stream and its connection, or answer
    /// false when either is full.
    fn admit(&self, len: usize) -> bool {
        let len = len as u64;
        let charged = self
            .queued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |q| {
                (q + len <= self.limit).then_some(q + len)
            })
            .is_ok();
        if !charged {
            return false;
        }
        if self
            .total
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                (t + len <= INBOUND_AGGREGATE_BYTES).then_some(t + len)
            })
            .is_err()
        {
            // Give the per-stream charge back: the two counters have to agree,
            // and a stream credited for bytes it never queued would leak its
            // window a chunk at a time.
            self.queued.fetch_sub(len, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn release(&self, len: usize) {
        self.queued.fetch_sub(len as u64, Ordering::Relaxed);
        self.total.fetch_sub(len as u64, Ordering::Relaxed);
    }
}

/// Connection-wide ceiling on client→target bytes buffered.
///
/// Deliberately *not* `NET_WINDOW_AGGREGATE`. The outbound direction holds to
/// 4 MiB because the server produces there and can park a reader until credit
/// frees; here the client produces, and the only lever on the dispatch loop is
/// refusal — waiting would stall every other stream and the terminal with
/// them. So the ceiling cannot be a figure a client honouring every window it
/// was granted can reach: the server reports those windows
/// (`msg_net_opened_tcp`), and closing such a stream would make the report a
/// lie.
///
/// That fixes the number rather than leaving it to taste. A live set of `m`
/// streams holds at most `per_stream_window(1) + … + per_stream_window(m)`,
/// because the j-th oldest of them had every earlier one open when it opened
/// and so was granted no more than `per_stream_window(j)`. At
/// `NET_MAX_SOCKETS` that sum is this constant — 39.9 MiB, against the 16 MiB
/// (one chunk per socket) it replaces, the difference being that the
/// per-stream floor is two chunks and not one.
///
/// It bounds bytes a client has sent that its *target* has not yet taken, so
/// reaching it needs 256 stalled targets at once; a target that drains keeps
/// the figure near zero. The alternative — charging grants against a smaller
/// budget and refusing an open that cannot be afforded a floor — bounds
/// memory lower but caps usable streams well below `NET_MAX_SOCKETS` and holds
/// a reservation for idle streams with nothing queued. Bounded and stated
/// (docs/design/net.md § Pacing) beats a number that reads tidier and cannot
/// hold.
const INBOUND_AGGREGATE_BYTES: u64 = every_grant_at_once();

/// The most a connection's live streams can have been granted between them.
const fn every_grant_at_once() -> u64 {
    let mut total = 0;
    let mut open = 1;
    while open <= NET_MAX_SOCKETS {
        total += per_stream_window(open);
        open += 1;
    }
    total
}

/// One stream's share of its connection's outstanding-bytes total.
///
/// A charge is released on drop rather than at each exit: the reader leaves
/// through an EOF, a reset, or an abort arriving in any of two `select!`s,
/// and a charge left behind shrinks the connection's aggregate window for as
/// long as the connection lives — a leak that looks like a stall.
struct CreditCharge {
    total: Arc<std::sync::atomic::AtomicU64>,
    credit: Arc<tokio::sync::Notify>,
    held: u64,
}

impl CreditCharge {
    fn new(total: Arc<std::sync::atomic::AtomicU64>, credit: Arc<tokio::sync::Notify>) -> Self {
        Self {
            total,
            credit,
            held: 0,
        }
    }

    /// Move this stream's charge to `want`, waking readers parked on the
    /// aggregate whenever it shrinks — our release may be their credit.
    fn set(&mut self, want: u64) {
        if want == self.held {
            return;
        }
        if want > self.held {
            self.total.fetch_add(want - self.held, Ordering::Relaxed);
        } else {
            self.total.fetch_sub(self.held - want, Ordering::Relaxed);
            self.credit.notify_waiters();
        }
        self.held = want;
    }
}

impl Drop for CreditCharge {
    fn drop(&mut self) {
        if self.held > 0 {
            self.total.fetch_sub(self.held, Ordering::Relaxed);
            self.credit.notify_waiters();
        }
    }
}

/// Per-stream credit, allocated from the aggregate so N streams cannot each claim a full window. Reported to the client on the accept, since only the server knows how many sockets were open at this one's open time.
///
/// `const` so `INBOUND_AGGREGATE_BYTES` can be derived from it: the aggregate has
/// to cover every window this can grant, and a hand-copied figure is one edit
/// away from not.
const fn per_stream_window(open_sockets: usize) -> u64 {
    let share = NET_WINDOW_AGGREGATE
        / if open_sockets == 0 {
            1
        } else {
            open_sockets as u64
        };
    // `clamp` is not const.
    if share < NET_WINDOW_MIN {
        NET_WINDOW_MIN
    } else if share > NET_WINDOW_BYTES {
        NET_WINDOW_BYTES
    } else {
        share
    }
}

fn udp_idle_timeout() -> Duration {
    // Zero is refused: a flow with no idle timeout is a socket the server holds until the connection dies.
    std::env::var("BLIT_FORWARD_UDP_IDLE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(UDP_IDLE_DEFAULT)
}

/// Whether a socket table still holds `id` — used by tests and, in the connection task, to decide whether a shutdown notice is owed.
#[cfg(test)]
fn is_udp(sockets: &NetSockets, id: u16) -> Option<bool> {
    sockets.map.get(&id).map(|e| e.is_udp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::net::NET_OPEN_TLS;

    fn policy(patterns: &[&str]) -> Policy {
        unsafe {
            std::env::remove_var("BLIT_ALLOW_FORWARD_INSECURE");
            std::env::remove_var("BLIT_ALLOW_FORWARD");
        }
        let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        Policy::new(false, &owned)
    }

    /// The egress allowlist docs/design/net.md § Target policy specifies.
    /// Unrestricted by default; loopback always reachable; names, globs,
    /// addresses, CIDRs and port lists all bounded.
    #[test]
    fn allow_forward_patterns() {
        // No patterns: everything.
        let p = policy(&[]);
        assert!(p.permits_host("example.com", 443));
        assert!(p.permits_addr("example.com", "93.184.216.34:443".parse().unwrap()));

        // Loopback needs no rule, so a dev server always works.
        let p = policy(&["example.com"]);
        assert!(p.permits_host("localhost", 3000));
        assert!(p.permits_host("127.0.0.1", 3000));
        assert!(p.permits_host("::1", 3000));
        assert!(!p.permits_host("elsewhere.com", 3000));
        assert!(
            p.permits_host("EXAMPLE.COM", 3000),
            "names are ASCII-case-insensitive"
        );

        // Ports narrow a rule, singly and by range.
        let p = policy(&["build.internal:8080,9000-9010"]);
        assert!(p.permits_host("build.internal", 8080));
        assert!(p.permits_host("build.internal", 9005));
        assert!(!p.permits_host("build.internal", 8081));
        assert!(!p.permits_host("build.internal", 9011));

        // A suffix glob covers the suffix itself and anything under it,
        // and nothing that merely ends with the same letters.
        let p = policy(&["*.svc.internal"]);
        assert!(p.permits_host("svc.internal", 80));
        assert!(p.permits_host("api.svc.internal", 80));
        assert!(!p.permits_host("notsvc.internal", 80));
        assert!(!p.permits_host("svc.internal.evil.com", 80));

        // `*` is everything, which is the default said out loud.
        assert!(policy(&["*"]).permits_host("anything.example", 1));

        // An address rule matches a requested literal and a resolved one.
        let p = policy(&["10.1.2.3:80"]);
        assert!(p.permits_host("10.1.2.3", 80));
        assert!(!p.permits_host("10.1.2.4", 80));
        assert!(p.permits_addr("db.internal", "10.1.2.3:80".parse().unwrap()));
        assert!(!p.permits_addr("db.internal", "10.9.9.9:80".parse().unwrap()));

        // CIDR, including a non-byte-aligned prefix.
        let p = policy(&["10.0.0.0/9"]);
        assert!(p.permits_addr("h", "10.0.0.1:80".parse().unwrap()));
        assert!(p.permits_addr("h", "10.127.255.255:80".parse().unwrap()));
        assert!(!p.permits_addr("h", "10.128.0.1:80".parse().unwrap()));
        // A v4 rule must not cover a v4-mapped v6 address: that would be a
        // way around the allowlist.
        assert!(!p.permits_addr("h", "[::ffff:10.0.0.1]:80".parse().unwrap()));

        // IPv6 rules keep their colons; the port split must not eat them.
        let p = policy(&["[2001:db8::1]:443"]);
        assert!(p.permits_addr("h", "[2001:db8::1]:443".parse().unwrap()));
        assert!(!p.permits_addr("h", "[2001:db8::2]:443".parse().unwrap()));
        assert!(policy(&["2001:db8::/32"]).permits_addr("h", "[2001:db8:1::5]:9".parse().unwrap()));

        // A name rule authorizes whatever that name resolves to — the grant
        // an operator writing a glob is asking for (net.md is explicit that
        // a stricter reading needs a CIDR).
        let p = policy(&["*.svc.internal"]);
        assert!(p.permits_addr("api.svc.internal", "203.0.113.5:80".parse().unwrap()));
        assert!(!p.permits_addr("evil.com", "203.0.113.5:80".parse().unwrap()));

        // Every one of these is unparsable, so the allowlist is empty — and
        // an empty allowlist that was *asked for* must not read as
        // unrestricted. An operator who mistyped the flag gets loopback.
        let p = policy(&["*.", "host:", "host:notaport", "host:9-8", "10.0.0.0/33"]);
        assert!(!p.permits_host("host", 9), "a dropped rule permits nothing");
        assert!(
            !p.permits_host("example.com", 443),
            "all-unparsable patterns must not widen the policy"
        );
        assert!(p.permits_host("localhost", 9), "loopback still works");
    }

    #[tokio::test]
    async fn dgram_queue_drops_oldest_when_full() {
        let q = DgramQueue::new();
        for i in 0..NET_DGRAM_QUEUE + 5 {
            q.push(vec![i as u8]).await;
        }
        assert_eq!(q.dropped(), 5);
        // The survivors are the newest: the first popped is not payload 0.
        let first = q.pop().await.unwrap();
        assert_eq!(first, vec![5u8]);
    }

    #[tokio::test]
    async fn dgram_queue_drops_on_byte_cap() {
        let q = DgramQueue::new();
        let big = vec![0u8; DGRAM_QUEUE_BYTES / 2 + 1];
        q.push(big.clone()).await;
        q.push(big.clone()).await;
        assert_eq!(q.dropped(), 1);
    }

    #[tokio::test]
    async fn dgram_queue_pop_ends_after_close() {
        let q = DgramQueue::new();
        q.push(vec![1]).await;
        q.close();
        assert_eq!(q.pop().await, Some(vec![1]));
        assert_eq!(q.pop().await, None);
    }

    #[tokio::test]
    async fn insecure_is_refused_unless_the_operator_allowed_it() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let open = NetOpen {
            flags: NET_OPEN_TLS | blit_remote::net::NET_OPEN_INSECURE,
            ..NetOpen::tcp(1, "127.0.0.1", 443)
        };
        handle_net_message(
            &blit_remote::net::msg_net_open(&open),
            &mut sockets,
            &tx,
            &policy(&[]),
            false,
        )
        .await;
        let (id, status, _, detail) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(id, 1);
        assert_eq!(status, NET_STATUS_PERMISSION);
        assert!(detail.contains("allow-forward-insecure"));
        assert!(sockets.map.is_empty());
    }

    #[test]
    fn insecure_gate_is_off_by_default_and_opt_in() {
        unsafe { std::env::remove_var("BLIT_ALLOW_FORWARD_INSECURE") };
        assert!(!Policy::new(false, &[]).insecure_allowed());
        assert!(Policy::new(true, &[]).insecure_allowed());
    }

    #[test]
    fn alpn_is_offered_verbatim_and_never_invented() {
        // A relay must not substitute a protocol the client did not ask for: offering http/1.1 to a client that offered nothing would change what the target speaks back.
        let none = client_config(&NetOpen {
            flags: NET_OPEN_TLS,
            ..NetOpen::tcp(1, "example.test", 443)
        })
        .unwrap();
        assert!(none.alpn_protocols.is_empty());

        let both = client_config(&NetOpen {
            flags: NET_OPEN_TLS,
            alpn: vec!["h2".into(), "http/1.1".into()],
            ..NetOpen::tcp(1, "example.test", 443)
        })
        .unwrap();
        assert_eq!(
            both.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn empty_alpn_entries_are_dropped_not_offered() {
        // A zero-length ALPN protocol is illegal on the wire; drop it rather than letting rustls refuse the whole handshake.
        let config = client_config(&NetOpen {
            flags: NET_OPEN_TLS,
            alpn: vec![String::new(), "h2".into()],
            ..NetOpen::tcp(1, "example.test", 443)
        })
        .unwrap();
        assert_eq!(config.alpn_protocols, vec![b"h2".to_vec()]);
    }

    #[tokio::test]
    async fn open_rejects_invalid_flag_combination() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let open = NetOpen {
            flags: blit_remote::net::NET_OPEN_UDP | NET_OPEN_TLS,
            ..NetOpen::tcp(3, "127.0.0.1", 53)
        };
        handle_net_message(
            &blit_remote::net::msg_net_open(&open),
            &mut sockets,
            &tx,
            &policy(&[]),
            false,
        )
        .await;
        let (_, status, _, detail) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status, NET_STATUS_INVALID);
        assert_eq!(detail, "UDP with TLS");
    }

    #[tokio::test]
    async fn tcp_relay_round_trips_and_reports_eof() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).await.unwrap();
            sock.write_all(b"PONG").await.unwrap();
            // Closing here gives the relay an EOF to report.
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        let open = NetOpen::tcp(1, "127.0.0.1", addr.port());
        handle_net_message(
            &blit_remote::net::msg_net_open(&open),
            &mut sockets,
            &tx,
            &p,
            false,
        )
        .await;
        let (_, status, _, detail) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status, NET_STATUS_OK, "open failed: {detail}");
        assert_eq!(is_udp(&sockets, 1), Some(false));

        handle_net_message(
            &blit_remote::net::msg_net_data_c2s(1, b"PING!"),
            &mut sockets,
            &tx,
            &p,
            false,
        )
        .await;

        let mut got = Vec::new();
        let mut closed = None;
        while closed.is_none() {
            let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("relay went quiet")
                .unwrap();
            match msg[0] {
                blit_remote::net::S2C_NET_DATA => {
                    got.extend_from_slice(blit_remote::net::parse_net_data_s2c(&msg).unwrap().1);
                }
                blit_remote::net::S2C_NET_CLOSED => {
                    closed = Some(blit_remote::net::parse_net_closed(&msg).unwrap().1);
                }
                _ => {}
            }
        }
        assert_eq!(got, b"PONG");
        assert_eq!(closed, Some(NET_CLOSED_EOF));
    }

    #[tokio::test]
    async fn udp_flow_round_trips() {
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = target.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let (n, from) = target.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"query");
            target.send_to(b"answer", from).await.unwrap();
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        let open = NetOpen::udp(4, "127.0.0.1", addr.port());
        handle_net_message(
            &blit_remote::net::msg_net_open(&open),
            &mut sockets,
            &tx,
            &p,
            false,
        )
        .await;
        let (_, status, _, detail) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status, NET_STATUS_OK, "open failed: {detail}");
        assert_eq!(is_udp(&sockets, 4), Some(true));

        handle_net_message(
            &blit_remote::net::msg_net_dgram_c2s(4, b"query"),
            &mut sockets,
            &tx,
            &p,
            false,
        )
        .await;
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no datagram came back")
            .unwrap();
        let (id, payload) = blit_remote::net::parse_net_dgram_s2c(&msg).unwrap();
        assert_eq!(id, 4);
        assert_eq!(payload, b"answer");
    }

    #[tokio::test]
    async fn stream_write_on_udp_flow_closes_it() {
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = target.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        handle_net_message(
            &blit_remote::net::msg_net_open(&NetOpen::udp(5, "127.0.0.1", addr.port())),
            &mut sockets,
            &tx,
            &p,
            false,
        )
        .await;
        let _ = rx.recv().await;
        handle_net_message(
            &blit_remote::net::msg_net_data_c2s(5, b"wrong opcode"),
            &mut sockets,
            &tx,
            &p,
            false,
        )
        .await;
        let (_, reason, detail) =
            blit_remote::net::parse_net_closed(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(reason, blit_remote::net::NET_CLOSED_POLICY);
        assert!(detail.contains("UDP"));
        assert!(sockets.map.is_empty());
    }

    /// Accepting a NET_OPEN must not wait for the target. The dispatch loop
    /// that handles this message is the same one that reads keystrokes, so
    /// an open naming an unreachable host used to freeze the client's
    /// terminal — and every other stream on the connection — for the whole
    /// connect timeout.
    #[tokio::test]
    async fn open_does_not_wait_for_the_target() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        // Reserved for documentation (RFC 5737): routes nowhere, so the
        // connect runs until CONNECT_TIMEOUT.
        let open = blit_remote::net::msg_net_open(&NetOpen::tcp(3, "192.0.2.1", 9));

        let started = tokio::time::Instant::now();
        handle_net_message(&open, &mut sockets, &tx, &p, false).await;
        let dispatch = started.elapsed();
        assert!(
            dispatch < std::time::Duration::from_millis(500),
            "dispatch blocked for {dispatch:?} on an unreachable target"
        );
        // The stream exists from now: data pipelined behind the open lands in
        // its channel rather than being dropped for want of an entry.
        assert!(sockets.map.contains_key(&3));
        // And no reply has been invented on its behalf yet.
        assert!(rx.try_recv().is_err(), "the open is still in progress");
    }

    /// Every NET_OPEN gets exactly one reply. A TLS open whose TLS block is
    /// missing used to parse to `None` and be dropped, leaving the client in
    /// OPENING forever, even though net.md lists it as INVALID and the
    /// stream id is right there at a fixed offset.
    #[tokio::test]
    async fn tls_open_without_a_tls_block_is_refused_by_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        // A well-formed TCP open, then the TLS flag set without appending the
        // SNI/ALPN block the flag promises.
        let mut msg = blit_remote::net::msg_net_open(&NetOpen::tcp(7, "127.0.0.1", 1));
        msg[3] |= blit_remote::net::NET_OPEN_TLS;
        handle_net_message(&msg, &mut sockets, &tx, &p, false).await;

        let (id, status, _, detail) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(id, 7, "the reply must name the stream the client opened");
        assert_eq!(status, NET_STATUS_INVALID);
        assert!(!detail.is_empty(), "a refusal should say why");
        assert!(sockets.map.is_empty());
    }

    /// A stream the *server* closed — target EOF or reset — leaves its entry
    /// in the map, since a socket task cannot remove itself. Reusing the id
    /// after NET_CLOSED is documented as legal, and the liveness check used
    /// to run before the corpse sweep and refuse it.
    #[tokio::test]
    async fn stream_id_is_reusable_after_a_server_close() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and immediately drop: the target EOFs, so the close is
            // the server's, not the client's.
            while let Ok((sock, _)) = listener.accept().await {
                drop(sock);
            }
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        let open = blit_remote::net::msg_net_open(&NetOpen::tcp(4, "127.0.0.1", addr.port()));

        handle_net_message(&open, &mut sockets, &tx, &p, false).await;
        assert_eq!(
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap())
                .unwrap()
                .1,
            NET_STATUS_OK
        );
        // Wait for the EOF close to arrive before reusing the id.
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("close should arrive")
                .unwrap();
            if let Some((_, reason, _)) = blit_remote::net::parse_net_closed(&msg) {
                assert_eq!(reason, blit_remote::net::NET_CLOSED_EOF);
                break;
            }
        }

        handle_net_message(&open, &mut sockets, &tx, &p, false).await;
        let (_, status, _, detail) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status, NET_STATUS_OK, "reuse refused: {detail}");
    }

    /// A stream's own limit does not bound a connection: every stream here is
    /// given the full 1 MiB, which is more than the server grants at this
    /// concurrency, and the shared counter is what refuses.
    #[test]
    fn inbound_windows_share_a_connection_wide_ceiling() {
        let total = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let window = |limit: u64| InboundWindow {
            queued: std::sync::atomic::AtomicU64::new(0),
            limit,
            total: total.clone(),
        };

        // One stream is held to its own limit regardless of the aggregate.
        let a = window(NET_MAX_CHUNK as u64);
        assert!(a.admit(NET_MAX_CHUNK));
        assert!(!a.admit(1), "past its own window");
        a.release(NET_MAX_CHUNK);
        assert_eq!(total.load(Ordering::Relaxed), 0, "release frees both");

        // Enough streams, each inside its own generous window, must still not
        // take the connection past the ceiling.
        let streams: Vec<InboundWindow> = (0..NET_MAX_SOCKETS)
            .map(|_| window(NET_WINDOW_BYTES))
            .collect();
        let mut admitted = 0u64;
        let mut refused = false;
        for s in &streams {
            for _ in 0..(NET_WINDOW_BYTES as usize / NET_MAX_CHUNK) {
                if s.admit(NET_MAX_CHUNK) {
                    admitted += NET_MAX_CHUNK as u64;
                } else {
                    refused = true;
                }
            }
        }
        assert!(refused, "the ceiling must actually refuse");
        // Chunk-sized admissions cannot land exactly on the ceiling, so the
        // last chunk that fits is the last one taken.
        assert!(admitted <= INBOUND_AGGREGATE_BYTES);
        assert!(admitted + NET_MAX_CHUNK as u64 > INBOUND_AGGREGATE_BYTES);
        assert_eq!(total.load(Ordering::Relaxed), admitted);

        // A refusal must not have charged anything: a stream credited for
        // bytes it never queued leaks its window a chunk at a time.
        let sum: u64 = streams
            .iter()
            .map(|s| s.queued.load(Ordering::Relaxed))
            .sum();
        assert_eq!(sum, admitted, "per-stream must match the total");

        // And a client honouring its per-stream window is never refused below
        // the ceiling: every stream can still hold one full chunk.
        let fresh = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let each: Vec<InboundWindow> = (0..NET_MAX_SOCKETS)
            .map(|_| InboundWindow {
                queued: std::sync::atomic::AtomicU64::new(0),
                limit: NET_WINDOW_BYTES,
                total: fresh.clone(),
            })
            .collect();
        for (i, s) in each.iter().enumerate() {
            assert!(s.admit(NET_MAX_CHUNK), "stream {i} denied its first chunk");
        }
    }

    /// The promise, on the enforcement side: a client that fills every window
    /// it was granted, on as many streams as the server allows at once, is
    /// never refused. This pairing is the whole reason
    /// `INBOUND_AGGREGATE_BYTES` is derived from `per_stream_window`.
    #[test]
    fn filling_every_granted_window_is_never_refused() {
        let total = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Grants as a live set of `NET_MAX_SOCKETS` streams would hold them:
        // the j-th oldest was granted `per_stream_window(j)`.
        let streams: Vec<InboundWindow> = (1..=NET_MAX_SOCKETS)
            .map(|open| InboundWindow {
                queued: std::sync::atomic::AtomicU64::new(0),
                limit: per_stream_window(open),
                total: total.clone(),
            })
            .collect();
        for (i, s) in streams.iter().enumerate() {
            let mut held = 0u64;
            while held + NET_MAX_CHUNK as u64 <= s.limit {
                assert!(
                    s.admit(NET_MAX_CHUNK),
                    "stream {i} refused inside its window"
                );
                held += NET_MAX_CHUNK as u64;
            }
            let tail = (s.limit - held) as usize;
            if tail > 0 {
                assert!(s.admit(tail), "stream {i} refused the tail of its window");
            }
        }
        assert_eq!(total.load(Ordering::Relaxed), INBOUND_AGGREGATE_BYTES);
    }

    /// The client→target direction is bounded too. The queue feeding the
    /// target is unbounded so the writer can never deadlock, so a client that
    /// ignores its send window — or that pipelines NET_DATA behind an open to
    /// a host that will never answer, which no longer blocks the dispatch
    /// loop — would otherwise grow server memory without limit.
    #[tokio::test]
    async fn flooding_past_the_inbound_window_ends_the_stream() {
        // A target that accepts and then never reads, so nothing drains.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        let open = blit_remote::net::msg_net_open(&NetOpen::tcp(5, "127.0.0.1", addr.port()));
        handle_net_message(&open, &mut sockets, &tx, &p, false).await;
        assert_eq!(
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap())
                .unwrap()
                .1,
            NET_STATUS_OK
        );

        // Push well past the window without ever waiting for an ack.
        let window = per_stream_window(1) as usize;
        let chunk = vec![b'q'; NET_MAX_CHUNK];
        let mut sent = 0usize;
        let mut closed = None;
        while sent < window * 4 {
            handle_net_message(
                &blit_remote::net::msg_net_data_c2s(5, &chunk),
                &mut sockets,
                &tx,
                &p,
                false,
            )
            .await;
            sent += chunk.len();
            while let Ok(msg) = rx.try_recv() {
                if let Some((_, reason, detail)) = blit_remote::net::parse_net_closed(&msg) {
                    closed = Some((reason, detail));
                }
            }
            if closed.is_some() {
                break;
            }
        }
        let (reason, detail) = closed.expect("flooding must end the stream");
        assert_eq!(reason, NET_CLOSED_BUDGET);
        assert!(detail.contains("window"), "detail: {detail}");
        assert!(
            sent <= window + NET_MAX_CHUNK * 2,
            "accepted {sent} bytes against a {window}-byte window"
        );
        // And the stream is gone, so the memory it held is too.
        assert!(!sockets.map.contains_key(&5));
    }

    /// A client cannot buy credit it has not earned. Acking `u64::MAX`
    /// used to make `sent - ack` saturate to zero, so the window never
    /// closed: the relay read the target as fast as it would deliver and
    /// queued all of it into the connection's unbounded outbox.
    #[tokio::test]
    async fn forged_ack_does_not_open_the_window() {
        let window = per_stream_window(1) as usize;
        // Enough that an honest window must stop us well short.
        let total = window * 4;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let chunk = vec![b'z'; 64 * 1024];
            let mut written = 0usize;
            while written < total {
                if tokio::io::AsyncWriteExt::write_all(&mut sock, &chunk)
                    .await
                    .is_err()
                {
                    return;
                }
                written += chunk.len();
            }
            // Hold the socket open: an EOF would let the reader finish for
            // reasons unrelated to pacing.
            std::future::pending::<()>().await;
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        let open = blit_remote::net::msg_net_open(&NetOpen::tcp(1, "127.0.0.1", addr.port()));
        handle_net_message(&open, &mut sockets, &tx, &p, false).await;
        assert_eq!(
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap())
                .unwrap()
                .1,
            NET_STATUS_OK
        );

        // The lie.
        let forged = blit_remote::net::msg_net_ack_c2s(1, u64::MAX);
        handle_net_message(&forged, &mut sockets, &tx, &p, false).await;

        // The relay must refuse the credit and end the stream rather than
        // read the target dry.
        let mut relayed = 0usize;
        let mut closed = None;
        while let Ok(Some(msg)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
        {
            if let Some((_, payload)) = blit_remote::net::parse_net_data_s2c(&msg) {
                relayed += payload.len();
            } else if let Some((_, reason, detail)) = blit_remote::net::parse_net_closed(&msg) {
                closed = Some((reason, detail));
                break;
            }
        }
        let (reason, detail) = closed.expect("a forged ack must close the stream");
        assert_eq!(reason, blit_remote::net::NET_CLOSED_POLICY);
        assert!(detail.contains("ahead of bytes sent"), "detail: {detail}");
        assert!(
            relayed <= window + NET_MAX_CHUNK,
            "forged ack granted {relayed} bytes against a {window}-byte window"
        );
    }

    #[tokio::test]
    async fn duplicate_live_stream_id_is_invalid() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        let open = blit_remote::net::msg_net_open(&NetOpen::tcp(9, "127.0.0.1", addr.port()));
        handle_net_message(&open, &mut sockets, &tx, &p, false).await;
        assert_eq!(
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap())
                .unwrap()
                .1,
            NET_STATUS_OK
        );
        handle_net_message(&open, &mut sockets, &tx, &p, false).await;
        let (_, status, _, detail) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status, NET_STATUS_INVALID);
        assert!(detail.contains("already live"));
    }

    #[tokio::test]
    async fn refuse_answers_open_when_family_is_disabled() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        refuse_net_message(
            &blit_remote::net::msg_net_open(&NetOpen::tcp(1, "127.0.0.1", 80)),
            &tx,
        );
        let (_, status, _, _) =
            blit_remote::net::parse_net_opened(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status, NET_STATUS_PERMISSION);
    }

    #[test]
    fn per_stream_window_shrinks_with_pressure() {
        assert_eq!(per_stream_window(1), NET_WINDOW_BYTES);
        assert!(per_stream_window(64) < NET_WINDOW_BYTES);
        // Never below two chunks, or a stream could not make progress.
        assert!(per_stream_window(10_000) >= NET_WINDOW_MIN);
    }

    /// The window the server reports is a promise, and this is what keeps it: a
    /// client that fills every window it was granted, on as many streams as it
    /// is allowed, never reaches the connection's inbound aggregate. The bound
    /// is the sum over a live set, because the j-th oldest live stream saw at
    /// least j sockets open when it opened (see `INBOUND_AGGREGATE_BYTES`).
    #[test]
    fn granted_windows_fit_inside_the_inbound_aggregate() {
        let summed = |sockets: usize| -> u64 { (1..=sockets).map(per_stream_window).sum() };
        for sockets in [1usize, 4, 5, 24, 64, 65, 128, NET_MAX_SOCKETS] {
            assert!(
                summed(sockets) <= INBOUND_AGGREGATE_BYTES,
                "{sockets} sockets granted {} against a {INBOUND_AGGREGATE_BYTES}-byte aggregate",
                summed(sockets)
            );
        }
        // Exactly covered, not generously: the aggregate is that sum, so it
        // cannot drift from the grant formula, and the figure is worth stating.
        assert_eq!(summed(NET_MAX_SOCKETS), INBOUND_AGGREGATE_BYTES);
        // Worth stating, since it is the memory a connection may hold: ~40 MiB.
        assert!((39 << 20..40 << 20).contains(&INBOUND_AGGREGATE_BYTES));
        // A stream still cannot hold more than its own window, so the aggregate
        // is not a way around one.
        assert!(INBOUND_AGGREGATE_BYTES > NET_MAX_SOCKETS as u64 * NET_MAX_CHUNK as u64);
    }

    /// The value the client needs and cannot compute: which window this stream
    /// got, given how many were already open.
    #[tokio::test]
    async fn accept_reports_the_window_it_granted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut sockets = NetSockets::default();
        let p = policy(&[]);
        // Five streams: the fifth is the first whose share is below the 1 MiB
        // a client used to assume.
        for id in 1..=5u16 {
            let open = blit_remote::net::msg_net_open(&NetOpen::tcp(id, "127.0.0.1", addr.port()));
            handle_net_message(&open, &mut sockets, &tx, &p, false).await;
            let reported = loop {
                let msg = rx.recv().await.unwrap();
                if let Some(window) = blit_remote::net::parse_net_opened_window(&msg) {
                    break window;
                }
            };
            assert_eq!(reported, per_stream_window(id as usize));
        }
        assert!(per_stream_window(5) < NET_WINDOW_BYTES);
    }
}
