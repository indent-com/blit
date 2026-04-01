use hmac::{Hmac, Mac};
use md5::{Digest as Md5Digest, Md5};
use sha1::Sha1;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const MAGIC_COOKIE: u32 = 0x2112A442;

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const ALLOCATE_REQUEST: u16 = 0x0003;
const ALLOCATE_RESPONSE: u16 = 0x0103;
const ALLOCATE_ERROR: u16 = 0x0113;
const CREATE_PERM_REQUEST: u16 = 0x0008;
const CREATE_PERM_RESPONSE: u16 = 0x0108;
const REFRESH_REQUEST: u16 = 0x0004;
const REFRESH_RESPONSE: u16 = 0x0104;
const SEND_INDICATION: u16 = 0x0016;
const DATA_INDICATION: u16 = 0x0017;

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4 * 60);
const PERMISSION_LIFETIME: std::time::Duration = std::time::Duration::from_secs(4 * 60);

fn txn_id() -> [u8; 12] {
    let u = uuid::Uuid::new_v4();
    let mut id = [0u8; 12];
    id.copy_from_slice(&u.as_bytes()[..12]);
    id
}

fn long_term_key(username: &str, realm: &str, password: &str) -> Vec<u8> {
    let mut h = Md5::new();
    h.update(format!("{username}:{realm}:{password}").as_bytes());
    h.finalize().to_vec()
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn xor_addr_encode(addr: SocketAddr, tid: &[u8; 12]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.push(0);
    match addr {
        SocketAddr::V4(v4) => {
            buf.push(0x01);
            let xport = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
            buf.extend_from_slice(&xport.to_be_bytes());
            let xaddr = u32::from_be_bytes(v4.ip().octets()) ^ MAGIC_COOKIE;
            buf.extend_from_slice(&xaddr.to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            buf.push(0x02);
            let xport = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
            buf.extend_from_slice(&xport.to_be_bytes());
            let ip_bytes = v6.ip().octets();
            let mut xor_key = [0u8; 16];
            xor_key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            xor_key[4..].copy_from_slice(tid);
            for i in 0..16 {
                buf.push(ip_bytes[i] ^ xor_key[i]);
            }
        }
    }
    buf
}

fn xor_addr_decode(data: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    if data.len() < 4 {
        return None;
    }
    let family = data[1];
    let xport = u16::from_be_bytes([data[2], data[3]]);
    let port = xport ^ (MAGIC_COOKIE >> 16) as u16;
    match family {
        0x01 => {
            if data.len() < 8 {
                return None;
            }
            let xaddr = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            Some(SocketAddr::new(Ipv4Addr::from(xaddr ^ MAGIC_COOKIE).into(), port))
        }
        0x02 => {
            if data.len() < 20 {
                return None;
            }
            let mut xor_key = [0u8; 16];
            xor_key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            xor_key[4..].copy_from_slice(tid);
            let mut ip = [0u8; 16];
            for i in 0..16 {
                ip[i] = data[4 + i] ^ xor_key[i];
            }
            Some(SocketAddr::new(Ipv6Addr::from(ip).into(), port))
        }
        _ => None,
    }
}

struct StunWriter {
    msg_type: u16,
    tid: [u8; 12],
    attrs: Vec<u8>,
}

impl StunWriter {
    fn new(msg_type: u16) -> Self {
        Self { msg_type, tid: txn_id(), attrs: Vec::new() }
    }

    fn attr(&mut self, atype: u16, value: &[u8]) {
        self.attrs.extend_from_slice(&atype.to_be_bytes());
        self.attrs.extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.attrs.extend_from_slice(value);
        let pad = (4 - (value.len() % 4)) % 4;
        self.attrs.extend(std::iter::repeat_n(0, pad));
    }

    fn build(self) -> Vec<u8> {
        let mut msg = Vec::with_capacity(20 + self.attrs.len());
        msg.extend_from_slice(&self.msg_type.to_be_bytes());
        msg.extend_from_slice(&(self.attrs.len() as u16).to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&self.tid);
        msg.extend_from_slice(&self.attrs);
        msg
    }

    fn build_with_integrity(self, key: &[u8]) -> Vec<u8> {
        let total_attr_len = self.attrs.len() + 24;
        let mut msg = Vec::with_capacity(20 + total_attr_len);
        msg.extend_from_slice(&self.msg_type.to_be_bytes());
        msg.extend_from_slice(&(total_attr_len as u16).to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&self.tid);
        msg.extend_from_slice(&self.attrs);
        let hmac = hmac_sha1(key, &msg);
        msg.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        msg.extend_from_slice(&20u16.to_be_bytes());
        msg.extend_from_slice(&hmac);
        msg
    }

    fn tid(&self) -> [u8; 12] {
        self.tid
    }
}

fn parse_attrs(data: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut attrs = Vec::new();
    let mut off = 0;
    while off + 4 <= data.len() {
        let atype = u16::from_be_bytes([data[off], data[off + 1]]);
        let alen = u16::from_be_bytes([data[off + 2], data[off + 3]]) as usize;
        off += 4;
        if off + alen > data.len() {
            break;
        }
        attrs.push((atype, data[off..off + alen].to_vec()));
        off += (alen + 3) & !3;
    }
    attrs
}

#[allow(clippy::type_complexity)]
fn parse_stun(data: &[u8]) -> Option<(u16, [u8; 12], Vec<(u16, Vec<u8>)>)> {
    if data.len() < 20 {
        return None;
    }
    if data[0] & 0xC0 != 0 {
        return None;
    }
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if u32::from_be_bytes([data[4], data[5], data[6], data[7]]) != MAGIC_COOKIE {
        return None;
    }
    let mut tid = [0u8; 12];
    tid.copy_from_slice(&data[8..20]);
    if data.len() < 20 + msg_len {
        return None;
    }
    let attrs = parse_attrs(&data[20..20 + msg_len]);
    Some((msg_type, tid, attrs))
}

fn build_send_indication(peer_addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let tid = txn_id();
    let mut w = StunWriter { msg_type: SEND_INDICATION, tid, attrs: Vec::new() };
    let xpa = xor_addr_encode(peer_addr, &tid);
    w.attr(ATTR_XOR_PEER_ADDRESS, &xpa);
    w.attr(ATTR_DATA, payload);
    w.build()
}

fn decode_data_indication(data: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
    let (mtype, tid, attrs) = parse_stun(data)?;
    if mtype != DATA_INDICATION {
        return None;
    }
    let mut peer_addr = None;
    let mut payload = None;
    for (atype, val) in &attrs {
        match *atype {
            ATTR_XOR_PEER_ADDRESS => peer_addr = xor_addr_decode(val, &tid),
            ATTR_DATA => payload = Some(val.clone()),
            _ => {}
        }
    }
    Some((peer_addr?, payload?))
}

// --- STUN binding ---

pub async fn stun_binding(
    server: SocketAddr,
    socket: &UdpSocket,
) -> Result<SocketAddr, Box<dyn std::error::Error + Send + Sync>> {
    let w = StunWriter::new(BINDING_REQUEST);
    let tid = w.tid();
    let msg = w.build();
    socket.send_to(&msg, server).await?;

    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("STUN binding timeout".into());
        }
        let (n, _) = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await??;
        if let Some((mtype, rtid, attrs)) = parse_stun(&buf[..n]) {
            if mtype == BINDING_RESPONSE && rtid == tid {
                for (atype, val) in &attrs {
                    if *atype == ATTR_XOR_MAPPED_ADDRESS {
                        if let Some(addr) = xor_addr_decode(val, &tid) {
                            return Ok(addr);
                        }
                    }
                }
                return Err("no XOR-MAPPED-ADDRESS".into());
            }
        }
    }
}

// --- TURN allocation (UDP) ---

async fn udp_allocate(
    socket: &UdpSocket,
    server: SocketAddr,
    username: &str,
    credential: &str,
) -> Result<(SocketAddr, Vec<u8>, String, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let mut w = StunWriter::new(ALLOCATE_REQUEST);
    w.attr(ATTR_REQUESTED_TRANSPORT, &[17, 0, 0, 0]);
    let tid = w.tid();
    socket.send_to(&w.build(), server).await?;

    let mut buf = [0u8; 1500];
    let (nonce, realm) = loop {
        let (n, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.recv_from(&mut buf),
        )
        .await??;
        if let Some((mtype, rtid, attrs)) = parse_stun(&buf[..n]) {
            if rtid == tid && mtype == ALLOCATE_ERROR {
                let mut nonce = None;
                let mut realm = None;
                for (atype, val) in &attrs {
                    match *atype {
                        ATTR_NONCE => nonce = Some(val.clone()),
                        ATTR_REALM => realm = Some(String::from_utf8_lossy(val).into_owned()),
                        _ => {}
                    }
                }
                break (
                    nonce.ok_or("no NONCE in 401")?,
                    realm.ok_or("no REALM in 401")?,
                );
            }
        }
    };

    let key = long_term_key(username, &realm, credential);
    let mut w2 = StunWriter::new(ALLOCATE_REQUEST);
    let tid2 = w2.tid();
    w2.attr(ATTR_REQUESTED_TRANSPORT, &[17, 0, 0, 0]);
    w2.attr(ATTR_USERNAME, username.as_bytes());
    w2.attr(ATTR_REALM, realm.as_bytes());
    w2.attr(ATTR_NONCE, &nonce);
    socket.send_to(&w2.build_with_integrity(&key), server).await?;

    loop {
        let (n, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.recv_from(&mut buf),
        )
        .await??;
        if let Some((mtype, rtid, attrs)) = parse_stun(&buf[..n]) {
            if rtid == tid2 {
                if mtype == ALLOCATE_RESPONSE {
                    let relay_addr = attrs
                        .iter()
                        .filter(|(t, _)| *t == ATTR_XOR_RELAYED_ADDRESS)
                        .find_map(|(_, val)| xor_addr_decode(val, &tid2));
                    match relay_addr {
                        Some(addr) => return Ok((addr, nonce, realm, key)),
                        None => return Err("no XOR-RELAYED-ADDRESS".into()),
                    }
                } else if mtype == ALLOCATE_ERROR {
                    let code = attrs
                        .iter()
                        .find(|(t, _)| *t == ATTR_ERROR_CODE)
                        .map(|(_, v)| {
                            if v.len() >= 4 {
                                (v[2] as u16) * 100 + v[3] as u16
                            } else {
                                0
                            }
                        })
                        .unwrap_or(0);
                    return Err(format!("TURN allocate error {code}").into());
                }
            }
        }
    }
}

async fn udp_create_permission(
    socket: &UdpSocket,
    server: SocketAddr,
    peer_addr: SocketAddr,
    nonce: &[u8],
    realm: &str,
    key: &[u8],
    username: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut w = StunWriter::new(CREATE_PERM_REQUEST);
    let tid = w.tid();
    let xpa = xor_addr_encode(peer_addr, &tid);
    w.attr(ATTR_XOR_PEER_ADDRESS, &xpa);
    w.attr(ATTR_USERNAME, username.as_bytes());
    w.attr(ATTR_REALM, realm.as_bytes());
    w.attr(ATTR_NONCE, nonce);
    socket.send_to(&w.build_with_integrity(key), server).await?;

    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        socket.recv_from(&mut buf),
    )
    .await??;
    if let Some((mtype, rtid, _)) = parse_stun(&buf[..n]) {
        if rtid == tid && mtype == CREATE_PERM_RESPONSE {
            return Ok(());
        }
    }
    Err("CreatePermission failed".into())
}

async fn udp_refresh(
    socket: &UdpSocket,
    server: SocketAddr,
    nonce: &[u8],
    realm: &str,
    key: &[u8],
    username: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut w = StunWriter::new(REFRESH_REQUEST);
    let tid = w.tid();
    w.attr(ATTR_LIFETIME, &600u32.to_be_bytes());
    w.attr(ATTR_USERNAME, username.as_bytes());
    w.attr(ATTR_REALM, realm.as_bytes());
    w.attr(ATTR_NONCE, nonce);
    socket.send_to(&w.build_with_integrity(key), server).await?;

    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        socket.recv_from(&mut buf),
    )
    .await??;
    if let Some((mtype, rtid, _)) = parse_stun(&buf[..n]) {
        if rtid == tid && mtype == REFRESH_RESPONSE {
            return Ok(());
        }
    }
    Err("TURN refresh failed".into())
}

#[allow(clippy::too_many_arguments)]
async fn udp_relay_task(
    socket: UdpSocket,
    server: SocketAddr,
    _relay_addr: SocketAddr,
    nonce: Vec<u8>,
    realm: String,
    key: Vec<u8>,
    username: String,
    mut send_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    recv_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
) {
    let mut buf = vec![0u8; 65535];
    let mut permitted = std::collections::HashSet::new();
    let mut refresh_timer = tokio::time::interval(REFRESH_INTERVAL);
    refresh_timer.tick().await;
    let mut perm_timer = tokio::time::interval(PERMISSION_LIFETIME);
    perm_timer.tick().await;

    loop {
        tokio::select! {
            msg = send_rx.recv() => {
                match msg {
                    Some((peer_addr, data)) => {
                        if !permitted.contains(&peer_addr.ip())
                            && udp_create_permission(
                                &socket, server, peer_addr, &nonce, &realm, &key, &username,
                            )
                            .await
                            .is_ok()
                        {
                            permitted.insert(peer_addr.ip());
                        }
                        let indication = build_send_indication(peer_addr, &data);
                        let _ = socket.send_to(&indication, server).await;
                    }
                    None => break,
                }
            }
            result = socket.recv_from(&mut buf) => {
                if let Ok((n, src)) = result {
                    if src == server {
                        if let Some((peer_addr, data)) = decode_data_indication(&buf[..n]) {
                            let _ = recv_tx.send((peer_addr, data));
                        }
                    }
                }
            }
            _ = refresh_timer.tick() => {
                if let Err(e) = udp_refresh(&socket, server, &nonce, &realm, &key, &username).await {
                    verbose!("TURN refresh failed: {e}");
                    break;
                }
            }
            _ = perm_timer.tick() => {
                permitted.clear();
            }
        }
    }
}

// --- TURN allocation (TCP/TLS) ---

enum TcpStream {
    Plain(tokio::net::TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

impl TcpStream {
    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.write_all(buf).await,
            Self::Tls(s) => s.write_all(buf).await,
        }
    }

    async fn read_stun_message(&mut self) -> std::io::Result<Vec<u8>> {
        let mut header = [0u8; 4];
        self.read_exact(&mut header).await?;
        let first_two = u16::from_be_bytes([header[0], header[1]]);
        if first_two & 0xC000 == 0x4000 {
            let data_len = u16::from_be_bytes([header[2], header[3]]) as usize;
            let padded = (data_len + 3) & !3;
            let mut data = vec![0u8; 4 + padded];
            data[..4].copy_from_slice(&header);
            self.read_exact(&mut data[4..]).await?;
            Ok(data)
        } else {
            let msg_len = u16::from_be_bytes([header[2], header[3]]) as usize;
            let mut msg = vec![0u8; 20 + msg_len];
            msg[..4].copy_from_slice(&header);
            self.read_exact(&mut msg[4..]).await?;
            Ok(msg)
        }
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => {
                tokio::io::AsyncReadExt::read_exact(s, buf).await?;
                Ok(())
            }
            Self::Tls(s) => {
                tokio::io::AsyncReadExt::read_exact(s, buf).await?;
                Ok(())
            }
        }
    }
}

async fn tcp_allocate(
    stream: &mut TcpStream,
    _server: SocketAddr,
    username: &str,
    credential: &str,
) -> Result<(SocketAddr, Vec<u8>, String, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    let mut w = StunWriter::new(ALLOCATE_REQUEST);
    w.attr(ATTR_REQUESTED_TRANSPORT, &[17, 0, 0, 0]);
    let tid = w.tid();
    stream.write_all(&w.build()).await?;

    let (nonce, realm) = loop {
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_stun_message(),
        )
        .await??;
        if let Some((mtype, rtid, attrs)) = parse_stun(&msg) {
            if rtid == tid && mtype == ALLOCATE_ERROR {
                let mut nonce = None;
                let mut realm = None;
                for (atype, val) in &attrs {
                    match *atype {
                        ATTR_NONCE => nonce = Some(val.clone()),
                        ATTR_REALM => realm = Some(String::from_utf8_lossy(val).into_owned()),
                        _ => {}
                    }
                }
                break (
                    nonce.ok_or("no NONCE in 401")?,
                    realm.ok_or("no REALM in 401")?,
                );
            }
        }
    };

    let key = long_term_key(username, &realm, credential);
    let mut w2 = StunWriter::new(ALLOCATE_REQUEST);
    let tid2 = w2.tid();
    w2.attr(ATTR_REQUESTED_TRANSPORT, &[17, 0, 0, 0]);
    w2.attr(ATTR_USERNAME, username.as_bytes());
    w2.attr(ATTR_REALM, realm.as_bytes());
    w2.attr(ATTR_NONCE, &nonce);
    stream.write_all(&w2.build_with_integrity(&key)).await?;

    loop {
        let msg = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_stun_message(),
        )
        .await??;
        if let Some((mtype, rtid, attrs)) = parse_stun(&msg) {
            if rtid == tid2 {
                if mtype == ALLOCATE_RESPONSE {
                    let relay_addr = attrs
                        .iter()
                        .filter(|(t, _)| *t == ATTR_XOR_RELAYED_ADDRESS)
                        .find_map(|(_, val)| xor_addr_decode(val, &tid2));
                    match relay_addr {
                        Some(addr) => return Ok((addr, nonce, realm, key)),
                        None => return Err("no XOR-RELAYED-ADDRESS".into()),
                    }
                } else if mtype == ALLOCATE_ERROR {
                    let code = attrs
                        .iter()
                        .find(|(t, _)| *t == ATTR_ERROR_CODE)
                        .map(|(_, v)| {
                            if v.len() >= 4 {
                                (v[2] as u16) * 100 + v[3] as u16
                            } else {
                                0
                            }
                        })
                        .unwrap_or(0);
                    return Err(format!("TURN allocate error {code}").into());
                }
            }
        }
    }
}

async fn tcp_create_permission(
    stream: &mut TcpStream,
    peer_addr: SocketAddr,
    nonce: &[u8],
    realm: &str,
    key: &[u8],
    username: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut w = StunWriter::new(CREATE_PERM_REQUEST);
    let tid = w.tid();
    let xpa = xor_addr_encode(peer_addr, &tid);
    w.attr(ATTR_XOR_PEER_ADDRESS, &xpa);
    w.attr(ATTR_USERNAME, username.as_bytes());
    w.attr(ATTR_REALM, realm.as_bytes());
    w.attr(ATTR_NONCE, nonce);
    stream.write_all(&w.build_with_integrity(key)).await?;

    let msg = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_stun_message(),
    )
    .await??;
    if let Some((mtype, rtid, _)) = parse_stun(&msg) {
        if rtid == tid && mtype == CREATE_PERM_RESPONSE {
            return Ok(());
        }
    }
    Err("CreatePermission failed".into())
}

async fn tcp_refresh(
    stream: &mut TcpStream,
    nonce: &[u8],
    realm: &str,
    key: &[u8],
    username: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut w = StunWriter::new(REFRESH_REQUEST);
    let tid = w.tid();
    w.attr(ATTR_LIFETIME, &600u32.to_be_bytes());
    w.attr(ATTR_USERNAME, username.as_bytes());
    w.attr(ATTR_REALM, realm.as_bytes());
    w.attr(ATTR_NONCE, nonce);
    stream.write_all(&w.build_with_integrity(key)).await?;

    let msg = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_stun_message(),
    )
    .await??;
    if let Some((mtype, rtid, _)) = parse_stun(&msg) {
        if rtid == tid && mtype == REFRESH_RESPONSE {
            return Ok(());
        }
    }
    Err("TURN refresh failed".into())
}

#[allow(clippy::too_many_arguments)]
async fn tcp_relay_task(
    mut stream: TcpStream,
    _relay_addr: SocketAddr,
    nonce: Vec<u8>,
    realm: String,
    key: Vec<u8>,
    username: String,
    mut send_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    recv_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
) {
    let mut permitted = std::collections::HashSet::new();
    let mut refresh_timer = tokio::time::interval(REFRESH_INTERVAL);
    refresh_timer.tick().await;
    let mut perm_timer = tokio::time::interval(PERMISSION_LIFETIME);
    perm_timer.tick().await;

    loop {
        tokio::select! {
            msg = send_rx.recv() => {
                match msg {
                    Some((peer_addr, data)) => {
                        if !permitted.contains(&peer_addr.ip())
                            && tcp_create_permission(
                                &mut stream, peer_addr, &nonce, &realm, &key, &username,
                            )
                            .await
                            .is_ok()
                        {
                            permitted.insert(peer_addr.ip());
                        }
                        let indication = build_send_indication(peer_addr, &data);
                        if stream.write_all(&indication).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            result = stream.read_stun_message() => {
                match result {
                    Ok(msg) => {
                        if let Some((peer_addr, data)) = decode_data_indication(&msg) {
                            let _ = recv_tx.send((peer_addr, data));
                        }
                    }
                    Err(_) => break,
                }
            }
            _ = refresh_timer.tick() => {
                if let Err(e) = tcp_refresh(&mut stream, &nonce, &realm, &key, &username).await {
                    verbose!("TURN refresh failed: {e}");
                    break;
                }
            }
            _ = perm_timer.tick() => {
                permitted.clear();
            }
        }
    }
}

// --- Public TurnRelay ---

pub struct TurnRelay {
    pub relay_addr: SocketAddr,
    #[allow(dead_code)]
    pub server_addr: SocketAddr,
    pub send_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    pub recv_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    _task: tokio::task::JoinHandle<()>,
}

impl TurnRelay {
    pub async fn allocate_udp(
        server_addr: SocketAddr,
        username: &str,
        credential: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let (relay_addr, nonce, realm, key) =
            udp_allocate(&socket, server_addr, username, credential).await?;

        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(udp_relay_task(
            socket,
            server_addr,
            relay_addr,
            nonce,
            realm,
            key,
            username.to_owned(),
            send_rx,
            recv_tx,
        ));

        Ok(Self { relay_addr, server_addr, send_tx, recv_rx, _task: task })
    }

    pub async fn allocate_tcp(
        server_addr: SocketAddr,
        tls: bool,
        hostname: &str,
        username: &str,
        credential: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let tcp = tokio::net::TcpStream::connect(server_addr).await?;
        let mut stream = if tls {
            let mut root_store = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                root_store.add(cert).ok();
            }
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
            let server_name = rustls::pki_types::ServerName::try_from(hostname.to_owned())?;
            let tls_stream = connector.connect(server_name, tcp).await?;
            TcpStream::Tls(Box::new(tls_stream))
        } else {
            TcpStream::Plain(tcp)
        };

        let (relay_addr, nonce, realm, key) =
            tcp_allocate(&mut stream, server_addr, username, credential).await?;

        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(tcp_relay_task(
            stream,
            relay_addr,
            nonce,
            realm,
            key,
            username.to_owned(),
            send_rx,
            recv_tx,
        ));

        Ok(Self { relay_addr, server_addr, send_tx, recv_rx, _task: task })
    }
}
