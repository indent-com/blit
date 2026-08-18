//! Native channel wire protocol (`docs/design/extensions.md`).
//!
//! Channels are a small process-global name registry plus full-duplex,
//! message-preserving byte streams. Both directions use opcode `0x95`; the
//! direction and sub-operation select the body schema.

use std::fmt;

/// Direction-local native-channel envelope opcode.
pub const CHANNEL: u8 = 0x95;
/// `S2C_HELLO` feature bit for native channels.
pub const FEATURE_CHANNEL: u32 = 1 << 12;
/// `S2C_HELLO` feature bit for `CHANNEL_WATCH`.
///
/// Its own bit rather than `FEATURE_CHANNEL`, because a `WATCH` an older
/// server does not know is an unknown sub-operation: the family's skip rule
/// drops it without a reply, which is indistinguishable from a name nobody
/// serves. A client that cannot see this bit has to keep probing by connect.
pub const FEATURE_CHANNEL_WATCH: u32 = 1 << 26;

pub const CHANNEL_LISTEN: u8 = 1;
pub const CHANNEL_CONNECT: u8 = 2;
pub const CHANNEL_DATA: u8 = 3;
pub const CHANNEL_ACK: u8 = 4;
pub const CHANNEL_CLOSE: u8 = 5;
pub const CHANNEL_WATCH: u8 = 6;
pub const CHANNEL_UNWATCH: u8 = 7;

pub const CHANNEL_OPENED: u8 = 1;
pub const CHANNEL_ACCEPTED: u8 = 2;
pub const CHANNEL_CLOSED: u8 = 5;
pub const CHANNEL_NAMES: u8 = 6;

/// `CONNECT.flags`: require the named listener to have this exact generation.
pub const CHANNEL_EXPECT_LISTENER_TOKEN: u8 = 1 << 0;

pub const CHANNEL_CLOSE_NORMAL: u8 = 0;
pub const CHANNEL_CLOSE_CANCELLED: u8 = 1;
pub const CHANNEL_CLOSE_PEER_GONE: u8 = 2;
pub const CHANNEL_CLOSE_PROTOCOL_VIOLATION: u8 = 3;
pub const CHANNEL_CLOSE_SERVER_SHUTDOWN: u8 = 4;

pub const CHANNEL_MAX_NAME: usize = 255;
pub const CHANNEL_MAX_PEER: usize = 255;
pub const CHANNEL_MAX_METADATA: usize = 64 * 1024;
pub const CHANNEL_MAX_PAYLOAD: usize = 1024 * 1024;
pub const CHANNEL_MAX_DETAIL: usize = 4 * 1024;
pub const CHANNEL_WINDOW_BYTES: u64 = 1024 * 1024;
pub const CHANNEL_MAX_UNCONSUMED_MESSAGES: usize = 1024;
/// Names one `CHANNEL_WATCH` may declare.
///
/// A watch names what it cares about instead of asking for the whole registry:
/// the reply is then bounded by the request, and the transient
/// `blit.cli.<ext>.<attempt>` listeners every extension mints cannot make a
/// watcher's traffic scale with churn it has no interest in.
pub const CHANNEL_MAX_WATCH_NAMES: usize = 32;

/// One decoded client-to-server channel operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelRequest<'a> {
    Listen {
        channel_id: u32,
        name: &'a str,
        metadata: &'a [u8],
    },
    Connect {
        channel_id: u32,
        name: &'a str,
        metadata: &'a [u8],
        listener_token: Option<[u8; 16]>,
    },
    Data {
        channel_id: u32,
        payload: &'a [u8],
    },
    Ack {
        channel_id: u32,
        bytes: u64,
    },
    Close {
        channel_id: u32,
        reason: u8,
    },
    /// Follow which of `names` currently have a listener, on a client-created
    /// ID that carries no stream and never accepts.
    Watch {
        channel_id: u32,
        names: Vec<&'a str>,
    },
    Unwatch {
        channel_id: u32,
    },
}

impl ChannelRequest<'_> {
    pub fn channel_id(&self) -> u32 {
        match self {
            Self::Listen { channel_id, .. }
            | Self::Connect { channel_id, .. }
            | Self::Data { channel_id, .. }
            | Self::Ack { channel_id, .. }
            | Self::Close { channel_id, .. }
            | Self::Watch { channel_id, .. }
            | Self::Unwatch { channel_id } => *channel_id,
        }
    }
}

/// One decoded server-to-client channel operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelMessage<'a> {
    Opened {
        channel_id: u32,
        status: u8,
        window: u64,
        peer: &'a str,
        metadata: &'a [u8],
        detail: &'a str,
    },
    Accepted {
        channel_id: u32,
        listener_id: u32,
        window: u64,
        peer: &'a str,
        metadata: &'a [u8],
    },
    Data {
        channel_id: u32,
        payload: &'a [u8],
    },
    Ack {
        channel_id: u32,
        bytes: u64,
    },
    Closed {
        channel_id: u32,
        reason: u8,
        detail: &'a str,
    },
    /// Which of a watch's declared names have a listener right now, in the
    /// order they were declared. A name the client asked about and does not
    /// find here has no listener; absence is the whole answer, so an empty
    /// list is meaningful rather than a no-op.
    Names {
        channel_id: u32,
        names: Vec<&'a str>,
    },
}

/// Structural or version-1 validation failure in a known channel operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelDecodeError {
    NotChannel,
    Truncated,
    TrailingBytes,
    InvalidFlags,
    InvalidClientId,
    InvalidName,
    InvalidUtf8,
    EmptyPayload,
    TooLarge,
    InvalidCloseReason,
    EmptyWatch,
    DuplicateWatchName,
}

impl fmt::Display for ChannelDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotChannel => "not a channel packet",
            Self::Truncated => "channel packet is truncated",
            Self::TrailingBytes => "channel packet has trailing bytes",
            Self::InvalidFlags => "channel flags are invalid",
            Self::InvalidClientId => "client-created channel id must be even",
            Self::InvalidName => "channel name is invalid",
            Self::InvalidUtf8 => "channel text is not valid UTF-8",
            Self::EmptyPayload => "channel data payload is empty",
            Self::TooLarge => "channel field exceeds its size limit",
            Self::InvalidCloseReason => "client channel close reason is invalid",
            Self::EmptyWatch => "channel watch declares no names",
            Self::DuplicateWatchName => "channel watch names must be distinct",
        })
    }
}

/// Read only the common envelope. Unknown kinds can then be skipped as one
/// complete packet, while malformed known kinds retain their channel ID for a
/// family-local response or close.
pub fn channel_header(packet: &[u8]) -> Result<(u8, u32, &[u8]), ChannelDecodeError> {
    if packet.first() != Some(&CHANNEL) {
        return Err(ChannelDecodeError::NotChannel);
    }
    if packet.len() < 6 {
        return Err(ChannelDecodeError::Truncated);
    }
    Ok((
        packet[1],
        u32::from_le_bytes(packet[2..6].try_into().expect("checked length")),
        &packet[6..],
    ))
}

/// Decode one C2S packet. `Ok(None)` is the protocol's unknown-kind skip rule.
pub fn parse_channel_request(
    packet: &[u8],
) -> Result<Option<ChannelRequest<'_>>, ChannelDecodeError> {
    let (kind, channel_id, body) = channel_header(packet)?;
    match kind {
        CHANNEL_LISTEN | CHANNEL_CONNECT => {
            if channel_id & 1 != 0 {
                return Err(ChannelDecodeError::InvalidClientId);
            }
            if body.len() < 3 {
                return Err(ChannelDecodeError::Truncated);
            }
            let flags = body[0];
            let name_len = u16::from_le_bytes([body[1], body[2]]) as usize;
            let name_end = 3usize
                .checked_add(name_len)
                .ok_or(ChannelDecodeError::TooLarge)?;
            let metadata_len_end = name_end
                .checked_add(4)
                .ok_or(ChannelDecodeError::TooLarge)?;
            if body.len() < metadata_len_end {
                return Err(ChannelDecodeError::Truncated);
            }
            let name = decode_name(&body[3..name_end])?;
            let metadata_len = u32::from_le_bytes(
                body[name_end..metadata_len_end]
                    .try_into()
                    .expect("checked length"),
            ) as usize;
            if metadata_len > CHANNEL_MAX_METADATA {
                return Err(ChannelDecodeError::TooLarge);
            }
            let metadata_end = metadata_len_end
                .checked_add(metadata_len)
                .ok_or(ChannelDecodeError::TooLarge)?;
            if body.len() < metadata_end {
                return Err(ChannelDecodeError::Truncated);
            }
            let metadata = &body[metadata_len_end..metadata_end];

            if kind == CHANNEL_LISTEN {
                if flags != 0 {
                    return Err(ChannelDecodeError::InvalidFlags);
                }
                if metadata_end != body.len() {
                    return Err(ChannelDecodeError::TrailingBytes);
                }
                Ok(Some(ChannelRequest::Listen {
                    channel_id,
                    name,
                    metadata,
                }))
            } else {
                if flags & !CHANNEL_EXPECT_LISTENER_TOKEN != 0 {
                    return Err(ChannelDecodeError::InvalidFlags);
                }
                let listener_token = if flags & CHANNEL_EXPECT_LISTENER_TOKEN != 0 {
                    let token_end = metadata_end
                        .checked_add(16)
                        .ok_or(ChannelDecodeError::TooLarge)?;
                    if body.len() < token_end {
                        return Err(ChannelDecodeError::Truncated);
                    }
                    if body.len() != token_end {
                        return Err(ChannelDecodeError::TrailingBytes);
                    }
                    Some(
                        body[metadata_end..token_end]
                            .try_into()
                            .expect("checked length"),
                    )
                } else {
                    if metadata_end != body.len() {
                        return Err(ChannelDecodeError::TrailingBytes);
                    }
                    None
                };
                Ok(Some(ChannelRequest::Connect {
                    channel_id,
                    name,
                    metadata,
                    listener_token,
                }))
            }
        }
        CHANNEL_DATA => {
            if body.is_empty() {
                return Err(ChannelDecodeError::EmptyPayload);
            }
            if body.len() > CHANNEL_MAX_PAYLOAD {
                return Err(ChannelDecodeError::TooLarge);
            }
            Ok(Some(ChannelRequest::Data {
                channel_id,
                payload: body,
            }))
        }
        CHANNEL_ACK => {
            if body.len() < 8 {
                return Err(ChannelDecodeError::Truncated);
            }
            if body.len() != 8 {
                return Err(ChannelDecodeError::TrailingBytes);
            }
            Ok(Some(ChannelRequest::Ack {
                channel_id,
                bytes: u64::from_le_bytes(body.try_into().expect("checked length")),
            }))
        }
        CHANNEL_CLOSE => {
            if body.is_empty() {
                return Err(ChannelDecodeError::Truncated);
            }
            if body.len() != 1 {
                return Err(ChannelDecodeError::TrailingBytes);
            }
            if body[0] > CHANNEL_CLOSE_CANCELLED {
                return Err(ChannelDecodeError::InvalidCloseReason);
            }
            Ok(Some(ChannelRequest::Close {
                channel_id,
                reason: body[0],
            }))
        }
        CHANNEL_WATCH => {
            if channel_id & 1 != 0 {
                return Err(ChannelDecodeError::InvalidClientId);
            }
            let names = decode_name_list(body, CHANNEL_MAX_WATCH_NAMES)?;
            if names.is_empty() {
                return Err(ChannelDecodeError::EmptyWatch);
            }
            // A repeated name would appear twice in a reply whose whole
            // meaning is which names are claimed, so the ambiguity is refused
            // rather than carried. The list is short enough that the obvious
            // scan is cheaper than a set.
            if names
                .iter()
                .enumerate()
                .any(|(index, name)| names[..index].contains(name))
            {
                return Err(ChannelDecodeError::DuplicateWatchName);
            }
            Ok(Some(ChannelRequest::Watch { channel_id, names }))
        }
        CHANNEL_UNWATCH => {
            if channel_id & 1 != 0 {
                return Err(ChannelDecodeError::InvalidClientId);
            }
            if !body.is_empty() {
                return Err(ChannelDecodeError::TrailingBytes);
            }
            Ok(Some(ChannelRequest::Unwatch { channel_id }))
        }
        _ => Ok(None),
    }
}

/// Decode one S2C packet. `Ok(None)` is the protocol's unknown-kind skip rule.
pub fn parse_channel_message(
    packet: &[u8],
) -> Result<Option<ChannelMessage<'_>>, ChannelDecodeError> {
    let (kind, channel_id, body) = channel_header(packet)?;
    match kind {
        CHANNEL_OPENED => {
            if body.len() < 15 {
                return Err(ChannelDecodeError::Truncated);
            }
            let status = body[0];
            let window = u64::from_le_bytes(body[1..9].try_into().expect("checked length"));
            let (peer, metadata, rest) = decode_peer_metadata(&body[9..])?;
            if rest.len() > CHANNEL_MAX_DETAIL {
                return Err(ChannelDecodeError::TooLarge);
            }
            let detail = std::str::from_utf8(rest).map_err(|_| ChannelDecodeError::InvalidUtf8)?;
            Ok(Some(ChannelMessage::Opened {
                channel_id,
                status,
                window,
                peer,
                metadata,
                detail,
            }))
        }
        CHANNEL_ACCEPTED => {
            if body.len() < 18 {
                return Err(ChannelDecodeError::Truncated);
            }
            let listener_id = u32::from_le_bytes(body[..4].try_into().expect("checked length"));
            let window = u64::from_le_bytes(body[4..12].try_into().expect("checked length"));
            let (peer, metadata, rest) = decode_peer_metadata(&body[12..])?;
            if !rest.is_empty() {
                return Err(ChannelDecodeError::TrailingBytes);
            }
            Ok(Some(ChannelMessage::Accepted {
                channel_id,
                listener_id,
                window,
                peer,
                metadata,
            }))
        }
        CHANNEL_DATA => {
            if body.is_empty() {
                return Err(ChannelDecodeError::EmptyPayload);
            }
            if body.len() > CHANNEL_MAX_PAYLOAD {
                return Err(ChannelDecodeError::TooLarge);
            }
            Ok(Some(ChannelMessage::Data {
                channel_id,
                payload: body,
            }))
        }
        CHANNEL_ACK => {
            if body.len() < 8 {
                return Err(ChannelDecodeError::Truncated);
            }
            if body.len() != 8 {
                return Err(ChannelDecodeError::TrailingBytes);
            }
            Ok(Some(ChannelMessage::Ack {
                channel_id,
                bytes: u64::from_le_bytes(body.try_into().expect("checked length")),
            }))
        }
        CHANNEL_CLOSED => {
            if body.is_empty() {
                return Err(ChannelDecodeError::Truncated);
            }
            if body.len() - 1 > CHANNEL_MAX_DETAIL {
                return Err(ChannelDecodeError::TooLarge);
            }
            let detail =
                std::str::from_utf8(&body[1..]).map_err(|_| ChannelDecodeError::InvalidUtf8)?;
            Ok(Some(ChannelMessage::Closed {
                channel_id,
                reason: body[0],
                detail,
            }))
        }
        CHANNEL_NAMES => Ok(Some(ChannelMessage::Names {
            channel_id,
            names: decode_name_list(body, CHANNEL_MAX_WATCH_NAMES)?,
        })),
        _ => Ok(None),
    }
}

pub fn msg_channel_listen(channel_id: u32, name: &str, metadata: &[u8]) -> Option<Vec<u8>> {
    msg_channel_open(CHANNEL_LISTEN, channel_id, name, metadata, None)
}

pub fn msg_channel_connect(
    channel_id: u32,
    name: &str,
    metadata: &[u8],
    listener_token: Option<[u8; 16]>,
) -> Option<Vec<u8>> {
    msg_channel_open(CHANNEL_CONNECT, channel_id, name, metadata, listener_token)
}

fn msg_channel_open(
    kind: u8,
    channel_id: u32,
    name: &str,
    metadata: &[u8],
    listener_token: Option<[u8; 16]>,
) -> Option<Vec<u8>> {
    if channel_id & 1 != 0 || decode_name(name.as_bytes()).is_err() {
        return None;
    }
    let metadata_len = u32::try_from(metadata.len()).ok()?;
    if metadata.len() > CHANNEL_MAX_METADATA {
        return None;
    }
    let flags = u8::from(listener_token.is_some()) * CHANNEL_EXPECT_LISTENER_TOKEN;
    let mut msg = envelope(kind, channel_id, 7 + name.len() + metadata.len() + 16);
    msg.push(flags);
    msg.extend_from_slice(&(name.len() as u16).to_le_bytes());
    msg.extend_from_slice(name.as_bytes());
    msg.extend_from_slice(&metadata_len.to_le_bytes());
    msg.extend_from_slice(metadata);
    if let Some(token) = listener_token {
        msg.extend_from_slice(&token);
    }
    Some(msg)
}

pub fn msg_channel_data(channel_id: u32, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.is_empty() || payload.len() > CHANNEL_MAX_PAYLOAD {
        return None;
    }
    let mut msg = envelope(CHANNEL_DATA, channel_id, payload.len());
    msg.extend_from_slice(payload);
    Some(msg)
}

pub fn msg_channel_ack(channel_id: u32, bytes: u64) -> Vec<u8> {
    let mut msg = envelope(CHANNEL_ACK, channel_id, 8);
    msg.extend_from_slice(&bytes.to_le_bytes());
    msg
}

pub fn msg_channel_close(channel_id: u32, reason: u8) -> Option<Vec<u8>> {
    if reason > CHANNEL_CLOSE_CANCELLED {
        return None;
    }
    let mut msg = envelope(CHANNEL_CLOSE, channel_id, 1);
    msg.push(reason);
    Some(msg)
}

pub fn msg_channel_opened(
    channel_id: u32,
    status: u8,
    window: u64,
    peer: &str,
    metadata: &[u8],
    detail: &str,
) -> Option<Vec<u8>> {
    if !valid_peer_name(peer)
        || metadata.len() > CHANNEL_MAX_METADATA
        || detail.len() > CHANNEL_MAX_DETAIL
    {
        return None;
    }
    let metadata_len = u32::try_from(metadata.len()).ok()?;
    let mut msg = envelope(
        CHANNEL_OPENED,
        channel_id,
        15 + peer.len() + metadata.len() + detail.len(),
    );
    msg.push(status);
    msg.extend_from_slice(&window.to_le_bytes());
    msg.extend_from_slice(&(peer.len() as u16).to_le_bytes());
    msg.extend_from_slice(peer.as_bytes());
    msg.extend_from_slice(&metadata_len.to_le_bytes());
    msg.extend_from_slice(metadata);
    msg.extend_from_slice(detail.as_bytes());
    Some(msg)
}

pub fn msg_channel_accepted(
    channel_id: u32,
    listener_id: u32,
    window: u64,
    peer: &str,
    metadata: &[u8],
) -> Option<Vec<u8>> {
    if !valid_peer_name(peer) || metadata.len() > CHANNEL_MAX_METADATA {
        return None;
    }
    let metadata_len = u32::try_from(metadata.len()).ok()?;
    let mut msg = envelope(
        CHANNEL_ACCEPTED,
        channel_id,
        18 + peer.len() + metadata.len(),
    );
    msg.extend_from_slice(&listener_id.to_le_bytes());
    msg.extend_from_slice(&window.to_le_bytes());
    msg.extend_from_slice(&(peer.len() as u16).to_le_bytes());
    msg.extend_from_slice(peer.as_bytes());
    msg.extend_from_slice(&metadata_len.to_le_bytes());
    msg.extend_from_slice(metadata);
    Some(msg)
}

pub fn msg_channel_watch(channel_id: u32, names: &[&str]) -> Option<Vec<u8>> {
    if channel_id & 1 != 0 || names.is_empty() {
        return None;
    }
    if names
        .iter()
        .enumerate()
        .any(|(index, name)| names[..index].contains(name))
    {
        return None;
    }
    encode_name_list(CHANNEL_WATCH, channel_id, names)
}

pub fn msg_channel_unwatch(channel_id: u32) -> Option<Vec<u8>> {
    if channel_id & 1 != 0 {
        return None;
    }
    Some(envelope(CHANNEL_UNWATCH, channel_id, 0))
}

/// The names of a watch that currently have a listener, in declared order.
pub fn msg_channel_names(channel_id: u32, names: &[&str]) -> Option<Vec<u8>> {
    encode_name_list(CHANNEL_NAMES, channel_id, names)
}

pub fn msg_channel_closed(channel_id: u32, reason: u8, detail: &str) -> Option<Vec<u8>> {
    if detail.len() > CHANNEL_MAX_DETAIL {
        return None;
    }
    let mut msg = envelope(CHANNEL_CLOSED, channel_id, 1 + detail.len());
    msg.push(reason);
    msg.extend_from_slice(detail.as_bytes());
    Some(msg)
}

fn envelope(kind: u8, channel_id: u32, body_capacity: usize) -> Vec<u8> {
    let mut msg = Vec::with_capacity(6 + body_capacity);
    msg.push(CHANNEL);
    msg.push(kind);
    msg.extend_from_slice(&channel_id.to_le_bytes());
    msg
}

/// `[flags:1][count:2]` then `count` × `[len:2][name]`, exactly.
///
/// Both directions of a watch carry a name list, and both need it to be
/// self-describing: the reply repeats the names rather than answering with a
/// bitmap over the request, so a client reading it needs no memory of what it
/// asked and a packet on the wire can be read on its own.
fn encode_name_list(kind: u8, channel_id: u32, names: &[&str]) -> Option<Vec<u8>> {
    if names.len() > CHANNEL_MAX_WATCH_NAMES {
        return None;
    }
    let count = u16::try_from(names.len()).ok()?;
    let bytes: usize = names.iter().map(|name| 2 + name.len()).sum();
    let mut msg = envelope(kind, channel_id, 3 + bytes);
    msg.push(0);
    msg.extend_from_slice(&count.to_le_bytes());
    for name in names {
        if decode_name(name.as_bytes()).is_err() {
            return None;
        }
        msg.extend_from_slice(&(name.len() as u16).to_le_bytes());
        msg.extend_from_slice(name.as_bytes());
    }
    Some(msg)
}

fn decode_name_list(body: &[u8], limit: usize) -> Result<Vec<&str>, ChannelDecodeError> {
    if body.len() < 3 {
        return Err(ChannelDecodeError::Truncated);
    }
    if body[0] != 0 {
        return Err(ChannelDecodeError::InvalidFlags);
    }
    let count = u16::from_le_bytes([body[1], body[2]]) as usize;
    if count > limit {
        return Err(ChannelDecodeError::TooLarge);
    }
    let mut names = Vec::with_capacity(count);
    let mut offset: usize = 3;
    for _ in 0..count {
        let length_end = offset.checked_add(2).ok_or(ChannelDecodeError::TooLarge)?;
        if body.len() < length_end {
            return Err(ChannelDecodeError::Truncated);
        }
        let length = u16::from_le_bytes([body[offset], body[offset + 1]]) as usize;
        let name_end = length_end
            .checked_add(length)
            .ok_or(ChannelDecodeError::TooLarge)?;
        if body.len() < name_end {
            return Err(ChannelDecodeError::Truncated);
        }
        names.push(decode_name(&body[length_end..name_end])?);
        offset = name_end;
    }
    if offset != body.len() {
        return Err(ChannelDecodeError::TrailingBytes);
    }
    Ok(names)
}

fn decode_name(bytes: &[u8]) -> Result<&str, ChannelDecodeError> {
    if bytes.is_empty() || bytes.len() > CHANNEL_MAX_NAME {
        return Err(ChannelDecodeError::InvalidName);
    }
    let name = std::str::from_utf8(bytes).map_err(|_| ChannelDecodeError::InvalidUtf8)?;
    if name.chars().any(char::is_control) {
        return Err(ChannelDecodeError::InvalidName);
    }
    Ok(name)
}

fn decode_peer_metadata(body: &[u8]) -> Result<(&str, &[u8], &[u8]), ChannelDecodeError> {
    if body.len() < 2 {
        return Err(ChannelDecodeError::Truncated);
    }
    let peer_len = u16::from_le_bytes([body[0], body[1]]) as usize;
    if peer_len > CHANNEL_MAX_PEER {
        return Err(ChannelDecodeError::TooLarge);
    }
    let peer_end = 2usize
        .checked_add(peer_len)
        .ok_or(ChannelDecodeError::TooLarge)?;
    let metadata_len_end = peer_end
        .checked_add(4)
        .ok_or(ChannelDecodeError::TooLarge)?;
    if body.len() < metadata_len_end {
        return Err(ChannelDecodeError::Truncated);
    }
    let peer =
        std::str::from_utf8(&body[2..peer_end]).map_err(|_| ChannelDecodeError::InvalidUtf8)?;
    if !valid_peer_name(peer) {
        return Err(ChannelDecodeError::InvalidUtf8);
    }
    let metadata_len = u32::from_le_bytes(
        body[peer_end..metadata_len_end]
            .try_into()
            .expect("checked length"),
    ) as usize;
    if metadata_len > CHANNEL_MAX_METADATA {
        return Err(ChannelDecodeError::TooLarge);
    }
    let metadata_end = metadata_len_end
        .checked_add(metadata_len)
        .ok_or(ChannelDecodeError::TooLarge)?;
    if body.len() < metadata_end {
        return Err(ChannelDecodeError::Truncated);
    }
    Ok((
        peer,
        &body[metadata_len_end..metadata_end],
        &body[metadata_end..],
    ))
}

pub fn valid_peer_name(peer: &str) -> bool {
    peer.len() <= CHANNEL_MAX_PEER
        && peer
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::STATUS_OK;

    #[test]
    fn listen_round_trip() {
        let wire = msg_channel_listen(42, "com.example.builder", b"meta").unwrap();
        assert_eq!(
            parse_channel_request(&wire).unwrap(),
            Some(ChannelRequest::Listen {
                channel_id: 42,
                name: "com.example.builder",
                metadata: b"meta",
            })
        );
    }

    #[test]
    fn token_checked_connect_round_trip() {
        let token = [7; 16];
        let wire = msg_channel_connect(2, "x", b"request", Some(token)).unwrap();
        assert_eq!(
            parse_channel_request(&wire).unwrap(),
            Some(ChannelRequest::Connect {
                channel_id: 2,
                name: "x",
                metadata: b"request",
                listener_token: Some(token),
            })
        );
    }

    #[test]
    fn opened_round_trip() {
        let wire = msg_channel_opened(
            4,
            STATUS_OK,
            CHANNEL_WINDOW_BYTES,
            "client:0000000000000001",
            b"listener metadata",
            "",
        )
        .unwrap();
        assert_eq!(
            parse_channel_message(&wire).unwrap(),
            Some(ChannelMessage::Opened {
                channel_id: 4,
                status: STATUS_OK,
                window: CHANNEL_WINDOW_BYTES,
                peer: "client:0000000000000001",
                metadata: b"listener metadata",
                detail: "",
            })
        );
    }

    #[test]
    fn accepted_round_trip() {
        let wire = msg_channel_accepted(
            3,
            8,
            CHANNEL_WINDOW_BYTES,
            "ext:0000000000000002:3",
            b"connector metadata",
        )
        .unwrap();
        assert_eq!(
            parse_channel_message(&wire).unwrap(),
            Some(ChannelMessage::Accepted {
                channel_id: 3,
                listener_id: 8,
                window: CHANNEL_WINDOW_BYTES,
                peer: "ext:0000000000000002:3",
                metadata: b"connector metadata",
            })
        );
    }

    #[test]
    fn data_ack_and_close_round_trip() {
        let data = msg_channel_data(2, b"hello").unwrap();
        assert_eq!(
            parse_channel_request(&data).unwrap(),
            Some(ChannelRequest::Data {
                channel_id: 2,
                payload: b"hello"
            })
        );
        let ack = msg_channel_ack(3, 5);
        assert_eq!(
            parse_channel_message(&ack).unwrap(),
            Some(ChannelMessage::Ack {
                channel_id: 3,
                bytes: 5
            })
        );
        let close = msg_channel_close(2, CHANNEL_CLOSE_CANCELLED).unwrap();
        assert_eq!(
            parse_channel_request(&close).unwrap(),
            Some(ChannelRequest::Close {
                channel_id: 2,
                reason: CHANNEL_CLOSE_CANCELLED
            })
        );
    }

    #[test]
    fn watch_round_trip() {
        let wire = msg_channel_watch(6, &["blit.session.v1", "blit.systemd.v1"]).unwrap();
        assert_eq!(
            parse_channel_request(&wire).unwrap(),
            Some(ChannelRequest::Watch {
                channel_id: 6,
                names: vec!["blit.session.v1", "blit.systemd.v1"],
            })
        );
        let unwatch = msg_channel_unwatch(6).unwrap();
        assert_eq!(
            parse_channel_request(&unwatch).unwrap(),
            Some(ChannelRequest::Unwatch { channel_id: 6 })
        );
    }

    #[test]
    fn names_round_trip_including_the_empty_answer() {
        let wire = msg_channel_names(6, &["blit.systemd.v1"]).unwrap();
        assert_eq!(
            parse_channel_message(&wire).unwrap(),
            Some(ChannelMessage::Names {
                channel_id: 6,
                names: vec!["blit.systemd.v1"],
            })
        );
        // Nothing claimed is an answer, not a packet to withhold.
        let empty = msg_channel_names(6, &[]).unwrap();
        assert_eq!(
            parse_channel_message(&empty).unwrap(),
            Some(ChannelMessage::Names {
                channel_id: 6,
                names: vec![],
            })
        );
    }

    #[test]
    fn watch_rejects_what_it_cannot_answer() {
        assert_eq!(msg_channel_watch(6, &[]), None);
        assert_eq!(msg_channel_watch(7, &["a"]), None);
        assert_eq!(msg_channel_watch(6, &["a", "a"]), None);
        assert_eq!(
            parse_channel_request(&[CHANNEL, CHANNEL_WATCH, 6, 0, 0, 0, 0, 0, 0]),
            Err(ChannelDecodeError::EmptyWatch)
        );
        let duplicated = [
            CHANNEL,
            CHANNEL_WATCH,
            6,
            0,
            0,
            0,
            0,
            2,
            0,
            1,
            0,
            b'a',
            1,
            0,
            b'a',
        ];
        assert_eq!(
            parse_channel_request(&duplicated),
            Err(ChannelDecodeError::DuplicateWatchName)
        );
        // An odd ID is server-created, and the server never watches.
        assert_eq!(
            parse_channel_request(&[CHANNEL, CHANNEL_UNWATCH, 7, 0, 0, 0]),
            Err(ChannelDecodeError::InvalidClientId)
        );
        assert_eq!(
            parse_channel_request(&[CHANNEL, CHANNEL_UNWATCH, 6, 0, 0, 0, 1]),
            Err(ChannelDecodeError::TrailingBytes)
        );
        // A count that outruns the body is truncation, not an empty list.
        assert_eq!(
            parse_channel_request(&[CHANNEL, CHANNEL_WATCH, 6, 0, 0, 0, 0, 1, 0]),
            Err(ChannelDecodeError::Truncated)
        );
        // Reserved flags stay reserved, so a future bit cannot be eaten.
        assert_eq!(
            parse_channel_message(&[CHANNEL, CHANNEL_NAMES, 6, 0, 0, 0, 1, 0, 0]),
            Err(ChannelDecodeError::InvalidFlags)
        );
    }

    #[test]
    fn unknown_kinds_are_skipped() {
        let wire = [CHANNEL, 99, 2, 0, 0, 0, 1, 2, 3];
        assert_eq!(parse_channel_request(&wire), Ok(None));
        assert_eq!(parse_channel_message(&wire), Ok(None));
    }

    #[test]
    fn malformed_known_operations_are_rejected() {
        assert_eq!(
            parse_channel_request(&[CHANNEL, CHANNEL_ACK, 2, 0, 0, 0, 1]),
            Err(ChannelDecodeError::Truncated)
        );
        assert_eq!(
            parse_channel_request(&[CHANNEL, CHANNEL_DATA, 2, 0, 0, 0]),
            Err(ChannelDecodeError::EmptyPayload)
        );
        assert!(msg_channel_listen(3, "x", b"").is_none());
        assert!(msg_channel_listen(2, "bad\nname", b"").is_none());
        assert!(msg_channel_close(2, CHANNEL_CLOSE_PEER_GONE).is_none());
    }

    #[test]
    fn feature_bit_is_distinct_from_shipped_families() {
        let taken = crate::FEATURE_CREATE_NONCE
            | crate::FEATURE_RESTART
            | crate::FEATURE_RESIZE_BATCH
            | crate::FEATURE_COPY_RANGE
            | crate::FEATURE_COMPOSITOR
            | crate::FEATURE_AUDIO
            | crate::fs::FEATURE_FS
            | crate::git::FEATURE_GIT
            | crate::lsp::FEATURE_LSP
            | crate::kv::FEATURE_KV
            | crate::net::FEATURE_NET
            | crate::FEATURE_CREATE_STATUS
            | crate::FEATURE_KILL_MODE
            | crate::FEATURE_PTY_DEADLINE
            | crate::FEATURE_SCROLL_BY
            | crate::FEATURE_SURFACE_TOUCH
            | crate::FEATURE_SURFACE_TEXT_INPUT
            | crate::FEATURE_CLIENT_CONTROL
            | crate::desktop::FEATURE_DESKTOP
            | crate::media::FEATURE_DESKTOP_MEDIA
            | crate::process::FEATURE_PROCESS
            | crate::process::FEATURE_PROCESS_SESSION_ENV
            | crate::process::FEATURE_APP_SOCKET
            | crate::env::FEATURE_ENV
            | crate::extension::FEATURE_EXTENSION;
        assert_eq!(FEATURE_CHANNEL & taken, 0);
        assert_eq!(FEATURE_CHANNEL_WATCH & (taken | FEATURE_CHANNEL), 0);
    }
}
