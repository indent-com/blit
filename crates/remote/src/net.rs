//! TCP and UDP relay wire protocol (docs/design/net.md).
//! A raw socket relay: the client names a host and port, the server opens a socket, and the two ends shuttle payload.

/// Open a socket: [0x80][stream_id:2][flags:1][port:2][host_len:2][host:N] + TLS block The TLS block ([sni_len:2][sni:N][alpn_count:1] repeated{[proto_len:1][proto:N]}) is present iff [`NET_OPEN_TLS`] is set.
pub const C2S_NET_OPEN: u8 = 0x80;
/// Stream payload, TCP only: [0x81][stream_id:2][data:N]
pub const C2S_NET_DATA: u8 = 0x81;
/// Cumulative byte-window credit, TCP only: [0x82][stream_id:2][bytes:8]
pub const C2S_NET_ACK: u8 = 0x82;
/// Close or half-close: [0x83][stream_id:2][flags:1]
pub const C2S_NET_CLOSE: u8 = 0x83;
/// One datagram, UDP only: [0x84][stream_id:2][payload:N]
pub const C2S_NET_DGRAM: u8 = 0x84;

/// Open result, one per `NET_OPEN`: [0x80][stream_id:2][status:1][alpn_len:1][alpn:N][detail_len:2][detail:N]
pub const S2C_NET_OPENED: u8 = 0x80;
/// Stream payload, TCP only: [0x81][stream_id:2][data:N]
pub const S2C_NET_DATA: u8 = 0x81;
/// Cumulative byte-window credit, TCP only: [0x82][stream_id:2][bytes:8]
pub const S2C_NET_ACK: u8 = 0x82;
/// Socket ended: [0x83][stream_id:2][reason:1][detail_len:2][detail:N] On a UDP flow `detail` carries the drop counts, both directions.
pub const S2C_NET_CLOSED: u8 = 0x83;
/// One datagram, UDP only: [0x84][stream_id:2][payload:N]
pub const S2C_NET_DGRAM: u8 = 0x84;

/// `S2C_HELLO` feature bit: server supports the `NET_*` family (docs/design/net.md).
pub const FEATURE_NET: u32 = 1 << 10;

// --------------------------------------------------------------------------- Flags ---------------------------------------------------------------------------

/// `NET_OPEN.flags` bit 0: terminate TLS toward the target; relayed bytes are the plaintext stream.
pub const NET_OPEN_TLS: u8 = 1 << 0;
/// `NET_OPEN.flags` bit 1: skip certificate and hostname verification.
pub const NET_OPEN_INSECURE: u8 = 1 << 1;
/// `NET_OPEN.flags` bit 2: open a UDP datagram flow rather than a TCP stream.
pub const NET_OPEN_UDP: u8 = 1 << 2;
/// Every defined `NET_OPEN` flag; anything else is `INVALID`.
pub const NET_OPEN_FLAGS_KNOWN: u8 = NET_OPEN_TLS | NET_OPEN_INSECURE | NET_OPEN_UDP;

/// `NET_CLOSE.flags` bit 0: shut down the client's write side only, leaving the stream readable.
pub const NET_CLOSE_WRITE: u8 = 1 << 0;

// --------------------------------------------------------------------------- Statuses and reasons ---------------------------------------------------------------------------

/// `NET_OPENED.status`.
pub const NET_STATUS_OK: u8 = 0;
/// `stream_id` unknown or already closed.
pub const NET_STATUS_UNKNOWN_ID: u8 = 1;
/// `host` did not resolve.
pub const NET_STATUS_NOT_FOUND: u8 = 2;
/// Connect refused, unreachable, or timed out.
pub const NET_STATUS_REFUSED: u8 = 3;
/// Target refused by policy (docs/design/net.md § Target policy).
pub const NET_STATUS_PERMISSION: u8 = 4;
/// TLS handshake or certificate verification failed.
pub const NET_STATUS_TLS: u8 = 5;
/// Concurrent-socket or memory budget exhausted.
pub const NET_STATUS_BUDGET: u8 = 6;
/// Malformed request: unknown flags, empty host, live `stream_id`, `INSECURE` without `TLS`, TLS block absent with `TLS` set, `UDP` combined with `TLS` or `INSECURE`.
pub const NET_STATUS_INVALID: u8 = 7;
/// Anything else; diagnostic in `detail`.
pub const NET_STATUS_OTHER: u8 = 9;

/// Human-readable form of a `NET_OPENED.status`, for CLI diagnostics.
pub fn net_status_text(status: u8) -> &'static str {
    match status {
        NET_STATUS_OK => "ok",
        NET_STATUS_UNKNOWN_ID => "unknown stream id",
        NET_STATUS_NOT_FOUND => "host did not resolve",
        NET_STATUS_REFUSED => "connection refused",
        NET_STATUS_PERMISSION => "refused by policy",
        NET_STATUS_TLS => "TLS failed",
        NET_STATUS_BUDGET => "budget exhausted",
        NET_STATUS_INVALID => "invalid request",
        NET_STATUS_OTHER => "backend error",
        _ => "unknown status",
    }
}

/// `NET_CLOSED.reason`: the target closed cleanly (TCP).
pub const NET_CLOSED_EOF: u8 = 0;
/// Connection reset by the target, or ICMP unreachable on a UDP flow.
pub const NET_CLOSED_RESET: u8 = 1;
/// Idle timeout — the normal end of a UDP flow.
pub const NET_CLOSED_TIMEOUT: u8 = 2;
/// Closed by the server: policy reload, target revoked.
pub const NET_CLOSED_POLICY: u8 = 3;
/// Retention or stream budget exceeded.
pub const NET_CLOSED_BUDGET: u8 = 4;
/// Server or blit connection going away.
pub const NET_CLOSED_SHUTDOWN: u8 = 5;

/// Human-readable form of a `NET_CLOSED.reason`, for CLI diagnostics.
pub fn net_closed_text(reason: u8) -> &'static str {
    match reason {
        NET_CLOSED_EOF => "closed",
        NET_CLOSED_RESET => "reset",
        NET_CLOSED_TIMEOUT => "idle timeout",
        NET_CLOSED_POLICY => "refused by policy",
        NET_CLOSED_BUDGET => "budget exceeded",
        NET_CLOSED_SHUTDOWN => "server going away",
        _ => "ended",
    }
}

// --------------------------------------------------------------------------- Limits ---------------------------------------------------------------------------

/// Maximum `host` length in bytes.
pub const NET_MAX_HOST: usize = 255;
/// Maximum `NET_DATA`/`NET_DGRAM` payload.
pub const NET_MAX_CHUNK: usize = 64 * 1024;
/// Maximum UDP payload — UDP's own limit, which fits inside [`NET_MAX_CHUNK`].
pub const NET_MAX_DGRAM: usize = 65507;
/// Default per-stream unacked-byte window.
pub const NET_WINDOW_BYTES: u64 = 1024 * 1024;
/// Default aggregate unacked-byte window across all of a connection's streams, so N streams cannot each claim a full window.
pub const NET_WINDOW_AGGREGATE: u64 = 4 * 1024 * 1024;
/// Maximum concurrent sockets per blit connection, streams and flows together.
pub const NET_MAX_SOCKETS: usize = 256;
/// Per-direction datagram queue depth on a UDP flow.
pub const NET_DGRAM_QUEUE: usize = 256;

// --------------------------------------------------------------------------- Message builders and parsers ---------------------------------------------------------------------------

/// A parsed `C2S_NET_OPEN`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetOpen {
    pub stream_id: u16,
    pub flags: u8,
    pub port: u16,
    pub host: String,
    /// Server Name Indication; empty means "use `host`".
    pub sni: String,
    /// ALPN protocols offered, in preference order.
    pub alpn: Vec<String>,
}

impl NetOpen {
    /// A plain TCP open: no TLS, no ALPN.
    pub fn tcp(stream_id: u16, host: &str, port: u16) -> Self {
        Self {
            stream_id,
            flags: 0,
            port,
            host: host.to_string(),
            sni: String::new(),
            alpn: Vec::new(),
        }
    }

    /// A UDP flow open.
    pub fn udp(stream_id: u16, host: &str, port: u16) -> Self {
        Self {
            stream_id,
            flags: NET_OPEN_UDP,
            port,
            host: host.to_string(),
            sni: String::new(),
            alpn: Vec::new(),
        }
    }

    pub fn is_udp(&self) -> bool {
        self.flags & NET_OPEN_UDP != 0
    }

    pub fn is_tls(&self) -> bool {
        self.flags & NET_OPEN_TLS != 0
    }

    /// The SNI to present: `sni` when set, otherwise `host`.
    pub fn effective_sni(&self) -> &str {
        if self.sni.is_empty() {
            &self.host
        } else {
            &self.sni
        }
    }

    /// Reject flag combinations the wire forbids, returning the `detail` for an `INVALID` reply.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.flags & !NET_OPEN_FLAGS_KNOWN != 0 {
            return Err("unknown flags");
        }
        if self.host.is_empty() {
            return Err("empty host");
        }
        if self.host.len() > NET_MAX_HOST {
            return Err("host too long");
        }
        if self.host.contains('\0') {
            return Err("host contains NUL");
        }
        if self.port == 0 {
            return Err("port must be non-zero");
        }
        if self.flags & NET_OPEN_INSECURE != 0 && self.flags & NET_OPEN_TLS == 0 {
            return Err("INSECURE without TLS");
        }
        if self.is_udp() && self.flags & (NET_OPEN_TLS | NET_OPEN_INSECURE) != 0 {
            return Err("UDP with TLS");
        }
        Ok(())
    }
}

pub fn msg_net_open(o: &NetOpen) -> Vec<u8> {
    let hb = o.host.as_bytes();
    let mut msg = Vec::with_capacity(8 + hb.len());
    msg.push(C2S_NET_OPEN);
    msg.extend_from_slice(&o.stream_id.to_le_bytes());
    msg.push(o.flags);
    msg.extend_from_slice(&o.port.to_le_bytes());
    msg.extend_from_slice(&(hb.len() as u16).to_le_bytes());
    msg.extend_from_slice(hb);
    if o.flags & NET_OPEN_TLS != 0 {
        let sb = o.sni.as_bytes();
        msg.extend_from_slice(&(sb.len() as u16).to_le_bytes());
        msg.extend_from_slice(sb);
        msg.push(o.alpn.len().min(u8::MAX as usize) as u8);
        for proto in o.alpn.iter().take(u8::MAX as usize) {
            let pb = proto.as_bytes();
            msg.push(pb.len().min(u8::MAX as usize) as u8);
            msg.extend_from_slice(&pb[..pb.len().min(u8::MAX as usize)]);
        }
    }
    msg
}

/// Parse a `C2S_NET_OPEN`.
pub fn parse_net_open(msg: &[u8]) -> Option<NetOpen> {
    if msg.len() < 8 || msg[0] != C2S_NET_OPEN {
        return None;
    }
    let stream_id = u16::from_le_bytes([msg[1], msg[2]]);
    let flags = msg[3];
    let port = u16::from_le_bytes([msg[4], msg[5]]);
    let host_len = u16::from_le_bytes([msg[6], msg[7]]) as usize;
    let host = std::str::from_utf8(msg.get(8..8 + host_len)?)
        .ok()?
        .to_string();
    let mut rest = &msg[8 + host_len..];
    let (sni, alpn) = if flags & NET_OPEN_TLS != 0 {
        if rest.len() < 2 {
            return None;
        }
        let sni_len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
        let sni = std::str::from_utf8(rest.get(2..2 + sni_len)?)
            .ok()?
            .to_string();
        rest = &rest[2 + sni_len..];
        let count = *rest.first()?;
        rest = &rest[1..];
        let mut alpn = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let len = *rest.first()? as usize;
            let proto = std::str::from_utf8(rest.get(1..1 + len)?).ok()?.to_string();
            rest = &rest[1 + len..];
            alpn.push(proto);
        }
        (sni, alpn)
    } else {
        (String::new(), Vec::new())
    };
    Some(NetOpen {
        stream_id,
        flags,
        port,
        host,
        sni,
        alpn,
    })
}

/// `C2S_NET_DATA` / `S2C_NET_DATA`, or the datagram variants — one builder, since the four differ only in opcode.
fn msg_payload(opcode: u8, stream_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3 + payload.len());
    msg.push(opcode);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(payload);
    msg
}

/// Split `[stream_id:2][payload:N]` off a data or datagram message.
fn parse_payload(msg: &[u8], opcode: u8) -> Option<(u16, &[u8])> {
    if msg.len() < 3 || msg[0] != opcode {
        return None;
    }
    Some((u16::from_le_bytes([msg[1], msg[2]]), &msg[3..]))
}

pub fn msg_net_data_c2s(stream_id: u16, data: &[u8]) -> Vec<u8> {
    msg_payload(C2S_NET_DATA, stream_id, data)
}

pub fn msg_net_data_s2c(stream_id: u16, data: &[u8]) -> Vec<u8> {
    msg_payload(S2C_NET_DATA, stream_id, data)
}

pub fn parse_net_data_c2s(msg: &[u8]) -> Option<(u16, &[u8])> {
    parse_payload(msg, C2S_NET_DATA)
}

pub fn parse_net_data_s2c(msg: &[u8]) -> Option<(u16, &[u8])> {
    parse_payload(msg, S2C_NET_DATA)
}

pub fn msg_net_dgram_c2s(stream_id: u16, payload: &[u8]) -> Vec<u8> {
    msg_payload(C2S_NET_DGRAM, stream_id, payload)
}

pub fn msg_net_dgram_s2c(stream_id: u16, payload: &[u8]) -> Vec<u8> {
    msg_payload(S2C_NET_DGRAM, stream_id, payload)
}

pub fn parse_net_dgram_c2s(msg: &[u8]) -> Option<(u16, &[u8])> {
    parse_payload(msg, C2S_NET_DGRAM)
}

pub fn parse_net_dgram_s2c(msg: &[u8]) -> Option<(u16, &[u8])> {
    parse_payload(msg, S2C_NET_DGRAM)
}

fn msg_ack(opcode: u8, stream_id: u16, bytes: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(11);
    msg.push(opcode);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.extend_from_slice(&bytes.to_le_bytes());
    msg
}

fn parse_ack(msg: &[u8], opcode: u8) -> Option<(u16, u64)> {
    if msg.len() < 11 || msg[0] != opcode {
        return None;
    }
    let stream_id = u16::from_le_bytes([msg[1], msg[2]]);
    let bytes = u64::from_le_bytes(msg[3..11].try_into().unwrap());
    Some((stream_id, bytes))
}

pub fn msg_net_ack_c2s(stream_id: u16, bytes: u64) -> Vec<u8> {
    msg_ack(C2S_NET_ACK, stream_id, bytes)
}

pub fn msg_net_ack_s2c(stream_id: u16, bytes: u64) -> Vec<u8> {
    msg_ack(S2C_NET_ACK, stream_id, bytes)
}

pub fn parse_net_ack_c2s(msg: &[u8]) -> Option<(u16, u64)> {
    parse_ack(msg, C2S_NET_ACK)
}

pub fn parse_net_ack_s2c(msg: &[u8]) -> Option<(u16, u64)> {
    parse_ack(msg, S2C_NET_ACK)
}

pub fn msg_net_close(stream_id: u16, flags: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4);
    msg.push(C2S_NET_CLOSE);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.push(flags);
    msg
}

/// Parse a `C2S_NET_CLOSE` → `(stream_id, flags)`.
pub fn parse_net_close(msg: &[u8]) -> Option<(u16, u8)> {
    if msg.len() < 4 || msg[0] != C2S_NET_CLOSE {
        return None;
    }
    Some((u16::from_le_bytes([msg[1], msg[2]]), msg[3]))
}

pub fn msg_net_opened(stream_id: u16, status: u8, alpn: &str, detail: &str) -> Vec<u8> {
    let ab = alpn.as_bytes();
    let db = detail.as_bytes();
    let mut msg = Vec::with_capacity(7 + ab.len() + db.len());
    msg.push(S2C_NET_OPENED);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.push(status);
    msg.push(ab.len().min(u8::MAX as usize) as u8);
    msg.extend_from_slice(&ab[..ab.len().min(u8::MAX as usize)]);
    msg.extend_from_slice(&(db.len() as u16).to_le_bytes());
    msg.extend_from_slice(db);
    msg
}

/// Parse an `S2C_NET_OPENED` → `(stream_id, status, alpn, detail)`.
pub fn parse_net_opened(msg: &[u8]) -> Option<(u16, u8, String, String)> {
    if msg.len() < 7 || msg[0] != S2C_NET_OPENED {
        return None;
    }
    let stream_id = u16::from_le_bytes([msg[1], msg[2]]);
    let status = msg[3];
    let alpn_len = msg[4] as usize;
    let alpn = std::str::from_utf8(msg.get(5..5 + alpn_len)?)
        .ok()?
        .to_string();
    let rest = &msg[5 + alpn_len..];
    if rest.len() < 2 {
        return None;
    }
    let detail_len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
    let detail = std::str::from_utf8(rest.get(2..2 + detail_len)?)
        .ok()?
        .to_string();
    Some((stream_id, status, alpn, detail))
}

pub fn msg_net_closed(stream_id: u16, reason: u8, detail: &str) -> Vec<u8> {
    let db = detail.as_bytes();
    let mut msg = Vec::with_capacity(6 + db.len());
    msg.push(S2C_NET_CLOSED);
    msg.extend_from_slice(&stream_id.to_le_bytes());
    msg.push(reason);
    msg.extend_from_slice(&(db.len() as u16).to_le_bytes());
    msg.extend_from_slice(db);
    msg
}

/// Parse an `S2C_NET_CLOSED` → `(stream_id, reason, detail)`.
pub fn parse_net_closed(msg: &[u8]) -> Option<(u16, u8, String)> {
    if msg.len() < 6 || msg[0] != S2C_NET_CLOSED {
        return None;
    }
    let stream_id = u16::from_le_bytes([msg[1], msg[2]]);
    let reason = msg[3];
    let detail_len = u16::from_le_bytes([msg[4], msg[5]]) as usize;
    let detail = std::str::from_utf8(msg.get(6..6 + detail_len)?)
        .ok()?
        .to_string();
    Some((stream_id, reason, detail))
}

/// True for any C2S opcode in the family's `0x80` block, for dispatch.
pub fn is_c2s_net(opcode: u8) -> bool {
    (C2S_NET_OPEN..=C2S_NET_DGRAM).contains(&opcode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text_distinguishes_other_from_unknown() {
        assert_eq!(net_status_text(NET_STATUS_OTHER), "backend error");
        assert_eq!(net_status_text(200), "unknown status");
    }

    #[test]
    fn open_roundtrip_plain() {
        let o = NetOpen::tcp(7, "db.internal", 5432);
        let parsed = parse_net_open(&msg_net_open(&o)).unwrap();
        assert_eq!(parsed, o);
        assert_eq!(parsed.validate(), Ok(()));
        assert!(!parsed.is_udp());
    }

    #[test]
    fn open_roundtrip_udp() {
        let o = NetOpen::udp(1, "resolver.internal", 53);
        let parsed = parse_net_open(&msg_net_open(&o)).unwrap();
        assert_eq!(parsed, o);
        assert!(parsed.is_udp());
        assert_eq!(parsed.validate(), Ok(()));
    }

    #[test]
    fn open_roundtrip_tls_with_alpn() {
        let o = NetOpen {
            stream_id: 9,
            flags: NET_OPEN_TLS,
            port: 443,
            host: "example.test".into(),
            sni: "other.test".into(),
            alpn: vec!["h2".into(), "http/1.1".into()],
        };
        let parsed = parse_net_open(&msg_net_open(&o)).unwrap();
        assert_eq!(parsed, o);
        assert_eq!(parsed.effective_sni(), "other.test");
    }

    #[test]
    fn empty_sni_falls_back_to_host() {
        let o = NetOpen {
            flags: NET_OPEN_TLS,
            ..NetOpen::tcp(1, "example.test", 443)
        };
        assert_eq!(o.effective_sni(), "example.test");
        assert_eq!(parse_net_open(&msg_net_open(&o)).unwrap(), o);
    }

    #[test]
    fn tls_block_may_offer_no_alpn() {
        let o = NetOpen {
            flags: NET_OPEN_TLS,
            ..NetOpen::tcp(2, "example.test", 443)
        };
        let parsed = parse_net_open(&msg_net_open(&o)).unwrap();
        assert!(parsed.alpn.is_empty());
    }

    #[test]
    fn truncated_tls_block_is_rejected() {
        let o = NetOpen {
            flags: NET_OPEN_TLS,
            alpn: vec!["h2".into()],
            ..NetOpen::tcp(3, "example.test", 443)
        };
        let full = msg_net_open(&o);
        // Every prefix that cuts into the TLS block must fail rather than silently decode as "TLS with no ALPN".
        for cut in 8 + "example.test".len()..full.len() {
            assert!(
                parse_net_open(&full[..cut]).is_none(),
                "prefix of len {cut} parsed"
            );
        }
        assert!(parse_net_open(&full).is_some());
    }

    #[test]
    fn validate_rejects_bad_combinations() {
        let udp_tls = NetOpen {
            flags: NET_OPEN_UDP | NET_OPEN_TLS,
            ..NetOpen::tcp(1, "h", 1)
        };
        assert_eq!(udp_tls.validate(), Err("UDP with TLS"));

        let insecure_only = NetOpen {
            flags: NET_OPEN_INSECURE,
            ..NetOpen::tcp(1, "h", 1)
        };
        assert_eq!(insecure_only.validate(), Err("INSECURE without TLS"));

        let unknown = NetOpen {
            flags: 1 << 5,
            ..NetOpen::tcp(1, "h", 1)
        };
        assert_eq!(unknown.validate(), Err("unknown flags"));

        let empty_host = NetOpen::tcp(1, "", 80);
        assert_eq!(empty_host.validate(), Err("empty host"));

        let zero_port = NetOpen::tcp(1, "h", 0);
        assert_eq!(zero_port.validate(), Err("port must be non-zero"));

        let long_host = NetOpen::tcp(1, &"x".repeat(NET_MAX_HOST + 1), 80);
        assert_eq!(long_host.validate(), Err("host too long"));
    }

    #[test]
    fn data_and_dgram_roundtrip() {
        assert_eq!(
            parse_net_data_c2s(&msg_net_data_c2s(4, b"hello")).unwrap(),
            (4, &b"hello"[..])
        );
        assert_eq!(
            parse_net_data_s2c(&msg_net_data_s2c(4, b"hello")).unwrap(),
            (4, &b"hello"[..])
        );
        assert_eq!(
            parse_net_dgram_c2s(&msg_net_dgram_c2s(5, b"query")).unwrap(),
            (5, &b"query"[..])
        );
        assert_eq!(
            parse_net_dgram_s2c(&msg_net_dgram_s2c(5, b"reply")).unwrap(),
            (5, &b"reply"[..])
        );
    }

    #[test]
    fn empty_payload_is_a_valid_datagram() {
        // A zero-length UDP datagram is legal and must survive the round trip as itself, not as a parse failure.
        let msg = msg_net_dgram_c2s(6, b"");
        assert_eq!(parse_net_dgram_c2s(&msg).unwrap(), (6, &b""[..]));
    }

    #[test]
    fn data_and_dgram_opcodes_do_not_cross_parse() {
        // Stream and datagram payloads must never decode as each other: that confusion is the whole reason they have separate opcodes.
        let data = msg_net_data_c2s(1, b"x");
        assert!(parse_net_dgram_c2s(&data).is_none());
        let dgram = msg_net_dgram_c2s(1, b"x");
        assert!(parse_net_data_c2s(&dgram).is_none());
        // Directions do share opcode numbers, as everywhere in the protocol — which way a message is going is context, not a tag.
        assert_eq!(C2S_NET_DATA, S2C_NET_DATA);
    }

    #[test]
    fn ack_roundtrip_beyond_32_bits() {
        let big = u64::MAX - 3;
        assert_eq!(
            parse_net_ack_c2s(&msg_net_ack_c2s(2, big)).unwrap(),
            (2, big)
        );
        assert_eq!(
            parse_net_ack_s2c(&msg_net_ack_s2c(2, big)).unwrap(),
            (2, big)
        );
    }

    #[test]
    fn close_roundtrip() {
        assert_eq!(
            parse_net_close(&msg_net_close(3, NET_CLOSE_WRITE)).unwrap(),
            (3, NET_CLOSE_WRITE)
        );
        assert_eq!(parse_net_close(&msg_net_close(3, 0)).unwrap(), (3, 0));
    }

    #[test]
    fn opened_roundtrip() {
        let msg = msg_net_opened(8, NET_STATUS_OK, "h2", "");
        assert_eq!(
            parse_net_opened(&msg).unwrap(),
            (8, NET_STATUS_OK, "h2".to_string(), String::new())
        );
        let msg = msg_net_opened(8, NET_STATUS_TLS, "", "unknown issuer");
        assert_eq!(
            parse_net_opened(&msg).unwrap(),
            (
                8,
                NET_STATUS_TLS,
                String::new(),
                "unknown issuer".to_string()
            )
        );
    }

    #[test]
    fn closed_roundtrip() {
        let msg = msg_net_closed(2, NET_CLOSED_TIMEOUT, "dropped 3 up, 0 down");
        assert_eq!(
            parse_net_closed(&msg).unwrap(),
            (2, NET_CLOSED_TIMEOUT, "dropped 3 up, 0 down".to_string())
        );
    }

    #[test]
    fn dispatch_range_covers_the_family_only() {
        for op in [
            C2S_NET_OPEN,
            C2S_NET_DATA,
            C2S_NET_ACK,
            C2S_NET_CLOSE,
            C2S_NET_DGRAM,
        ] {
            assert!(is_c2s_net(op));
        }
        assert!(!is_c2s_net(0x7F));
        assert!(!is_c2s_net(0x85));
        // The kv block below must stay outside the range.
        assert!(!is_c2s_net(crate::kv::C2S_KV_FETCH));
    }

    #[test]
    fn feature_bit_is_free() {
        for taken in [
            crate::fs::FEATURE_FS,
            crate::git::FEATURE_GIT,
            crate::lsp::FEATURE_LSP,
            crate::kv::FEATURE_KV,
        ] {
            assert_eq!(FEATURE_NET & taken, 0);
        }
    }

    #[test]
    fn dgram_cap_fits_the_chunk_cap() {
        const { assert!(NET_MAX_DGRAM <= NET_MAX_CHUNK) };
    }
}
