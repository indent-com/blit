//! Server environment wire protocol (docs/design/env.md).
//!
//! One request, one reply: a client asks for the server's environment and gets
//! every variable back, sorted by key. It exists because a client cannot
//! otherwise learn anything about the session it is attached to — not the
//! compositor socket, not `XDG_DATA_DIRS`, nothing — and an extension has no
//! host access at all beyond this protocol.
//!
//! This exposes whatever the server was started with, credentials included.
//! `BLIT_ENV=0` refuses the whole family at dispatch with `PERMISSION`; see the
//! security section of the design doc before widening anything here.
//!
//! All integers little-endian, tightly packed, as everywhere in the protocol.

use std::collections::BTreeMap;

/// Request the server environment: [0x75][nonce:2]
pub const C2S_ENV_GET: u8 = 0x75;

/// The environment: [0x75][nonce:2][status:1][count:2] then `count` records of
/// [key_len:2][key:N][value_len:4][value:N], ascending by key.
pub const S2C_ENV: u8 = 0x75;

/// `S2C_HELLO` feature bit: server answers `ENV_*`. `BLIT_ENV=0` refuses at
/// dispatch with `PERMISSION` rather than un-advertising, matching `FEATURE_KV`,
/// so a client can tell "this server has no such family" from "the operator
/// turned it off".
pub const FEATURE_ENV: u32 = 1 << 24;

/// Longest key accepted, in bytes.
pub const ENV_MAX_KEY: usize = 4 * 1024;
/// Longest value accepted, in bytes.
pub const ENV_MAX_VALUE: usize = 1024 * 1024;
/// Most variables one reply may carry.
pub const ENV_MAX_COUNT: usize = 8192;
/// Cap on the sum of all key and value bytes in one reply.
pub const ENV_MAX_TOTAL: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvCodecError {
    /// Truncated, trailing, or otherwise malformed.
    Invalid,
    /// A documented limit was exceeded.
    TooLarge,
}

impl EnvCodecError {
    pub fn status(self) -> u8 {
        match self {
            Self::Invalid => crate::STATUS_INVALID,
            Self::TooLarge => crate::STATUS_TOO_LARGE,
        }
    }
}

pub fn is_c2s_env(opcode: u8) -> bool {
    opcode == C2S_ENV_GET
}

/// Encode `C2S_ENV_GET`.
pub fn msg_env_get(nonce: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_ENV_GET);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg
}

/// Decode `C2S_ENV_GET`, returning the nonce.
pub fn parse_env_get(msg: &[u8]) -> Result<u16, EnvCodecError> {
    let body = body_of(msg, C2S_ENV_GET)?;
    if body.len() != 2 {
        return Err(EnvCodecError::Invalid);
    }
    Ok(u16::from_le_bytes([body[0], body[1]]))
}

/// Encode `S2C_ENV`. Entries are emitted in `BTreeMap` order, so the reply is
/// byte-identical for an unchanged environment.
///
/// Keys and values are raw bytes: a Unix environment is not required to be
/// UTF-8, and silently dropping a non-UTF-8 entry would be a worse answer than
/// handing it over as it is.
pub fn msg_env(
    nonce: u16,
    status: u8,
    entries: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<Vec<u8>, EnvCodecError> {
    if entries.len() > ENV_MAX_COUNT {
        return Err(EnvCodecError::TooLarge);
    }
    let mut total = 0usize;
    for (key, value) in entries {
        if key.is_empty() || key.len() > ENV_MAX_KEY || value.len() > ENV_MAX_VALUE {
            return Err(EnvCodecError::TooLarge);
        }
        // A NUL cannot survive a round trip through execve, so refuse to claim
        // it did.
        if key.contains(&0) || value.contains(&0) {
            return Err(EnvCodecError::Invalid);
        }
        total = total
            .checked_add(key.len())
            .and_then(|sum| sum.checked_add(value.len()))
            .ok_or(EnvCodecError::TooLarge)?;
        if total > ENV_MAX_TOTAL {
            return Err(EnvCodecError::TooLarge);
        }
    }
    let mut msg = Vec::with_capacity(6 + total + entries.len() * 6);
    msg.push(S2C_ENV);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (key, value) in entries {
        msg.extend_from_slice(&(key.len() as u16).to_le_bytes());
        msg.extend_from_slice(key);
        msg.extend_from_slice(&(value.len() as u32).to_le_bytes());
        msg.extend_from_slice(value);
    }
    Ok(msg)
}

/// A decoded `S2C_ENV`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvReply {
    pub nonce: u16,
    pub status: u8,
    pub entries: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// Decode `S2C_ENV`.
pub fn parse_env(msg: &[u8]) -> Result<EnvReply, EnvCodecError> {
    let mut body = body_of(msg, S2C_ENV)?;
    let nonce = take_u16(&mut body)?;
    let status = take_u8(&mut body)?;
    let count = take_u16(&mut body)? as usize;
    if count > ENV_MAX_COUNT {
        return Err(EnvCodecError::TooLarge);
    }
    let mut entries = BTreeMap::new();
    let mut total = 0usize;
    for _ in 0..count {
        let key_len = take_u16(&mut body)? as usize;
        if key_len == 0 || key_len > ENV_MAX_KEY {
            return Err(EnvCodecError::TooLarge);
        }
        let key = take_bytes(&mut body, key_len)?.to_vec();
        let value_len = take_u32(&mut body)? as usize;
        if value_len > ENV_MAX_VALUE {
            return Err(EnvCodecError::TooLarge);
        }
        let value = take_bytes(&mut body, value_len)?.to_vec();
        total = total
            .checked_add(key_len)
            .and_then(|sum| sum.checked_add(value_len))
            .ok_or(EnvCodecError::TooLarge)?;
        if total > ENV_MAX_TOTAL {
            return Err(EnvCodecError::TooLarge);
        }
        // A duplicate key would silently lose one of the two values, so it is
        // malformed rather than merged.
        if entries.insert(key, value).is_some() {
            return Err(EnvCodecError::Invalid);
        }
    }
    if !body.is_empty() {
        return Err(EnvCodecError::Invalid);
    }
    Ok(EnvReply {
        nonce,
        status,
        entries,
    })
}

fn body_of(msg: &[u8], opcode: u8) -> Result<&[u8], EnvCodecError> {
    match msg.split_first() {
        Some((&first, rest)) if first == opcode => Ok(rest),
        _ => Err(EnvCodecError::Invalid),
    }
}

fn take_bytes<'a>(body: &mut &'a [u8], len: usize) -> Result<&'a [u8], EnvCodecError> {
    if body.len() < len {
        return Err(EnvCodecError::Invalid);
    }
    let (head, rest) = body.split_at(len);
    *body = rest;
    Ok(head)
}

fn take_u8(body: &mut &[u8]) -> Result<u8, EnvCodecError> {
    Ok(take_bytes(body, 1)?[0])
}

fn take_u16(body: &mut &[u8]) -> Result<u16, EnvCodecError> {
    let bytes = take_bytes(body, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn take_u32(body: &mut &[u8]) -> Result<u32, EnvCodecError> {
    let bytes = take_bytes(body, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BTreeMap<Vec<u8>, Vec<u8>> {
        BTreeMap::from([
            (b"HOME".to_vec(), b"/home/pcarrier".to_vec()),
            (b"XDG_DATA_DIRS".to_vec(), b"/usr/share:/usr/local".to_vec()),
            // Empty values are legal and must survive.
            (b"EMPTY".to_vec(), Vec::new()),
        ])
    }

    #[test]
    fn allocation_is_locked() {
        assert_eq!(C2S_ENV_GET, 0x75);
        assert_eq!(S2C_ENV, 0x75);
        assert_eq!(FEATURE_ENV, 1 << 24);
    }

    #[test]
    fn a_request_round_trips() {
        assert_eq!(parse_env_get(&msg_env_get(0x1234)), Ok(0x1234));
        // Trailing bytes are refused, not ignored.
        let mut trailing = msg_env_get(1);
        trailing.push(0);
        assert_eq!(parse_env_get(&trailing), Err(EnvCodecError::Invalid));
    }

    #[test]
    fn a_reply_round_trips_and_is_deterministic() {
        let entries = sample();
        let encoded = msg_env(7, crate::STATUS_OK, &entries).expect("encodes");
        let decoded = parse_env(&encoded).expect("decodes");
        assert_eq!(decoded.nonce, 7);
        assert_eq!(decoded.status, crate::STATUS_OK);
        assert_eq!(decoded.entries, entries);
        // Same input, same bytes — the ordering is part of the contract.
        assert_eq!(
            encoded,
            msg_env(7, crate::STATUS_OK, &sample()).expect("encodes")
        );
    }

    /// A refusal carries no entries but still answers under the nonce, so a
    /// client waiting on one is never left hanging.
    #[test]
    fn a_refusal_is_a_well_formed_empty_reply() {
        let encoded = msg_env(9, crate::STATUS_PERMISSION, &BTreeMap::new()).expect("encodes");
        let decoded = parse_env(&encoded).expect("decodes");
        assert_eq!(decoded.status, crate::STATUS_PERMISSION);
        assert!(decoded.entries.is_empty());
        assert_eq!(decoded.nonce, 9);
    }

    #[test]
    fn malformed_replies_are_refused() {
        // Truncated mid-record.
        let encoded = msg_env(1, crate::STATUS_OK, &sample()).expect("encodes");
        for cut in 1..encoded.len() {
            assert!(
                parse_env(&encoded[..cut]).is_err(),
                "prefix of length {cut} must not decode"
            );
        }
        // Wrong opcode.
        let mut wrong = encoded.clone();
        wrong[0] = 0x76;
        assert_eq!(parse_env(&wrong), Err(EnvCodecError::Invalid));
        // Trailing bytes.
        let mut trailing = encoded;
        trailing.push(0xFF);
        assert_eq!(parse_env(&trailing), Err(EnvCodecError::Invalid));
    }

    #[test]
    fn a_nul_is_refused_rather_than_claimed_to_round_trip() {
        let with_nul = BTreeMap::from([(b"K".to_vec(), b"a\0b".to_vec())]);
        assert_eq!(
            msg_env(1, crate::STATUS_OK, &with_nul),
            Err(EnvCodecError::Invalid)
        );
        let empty_key = BTreeMap::from([(Vec::new(), b"v".to_vec())]);
        assert_eq!(
            msg_env(1, crate::STATUS_OK, &empty_key),
            Err(EnvCodecError::TooLarge)
        );
    }

    #[test]
    fn an_oversized_value_is_refused() {
        let big = BTreeMap::from([(b"K".to_vec(), vec![b'x'; ENV_MAX_VALUE + 1])]);
        assert_eq!(
            msg_env(1, crate::STATUS_OK, &big),
            Err(EnvCodecError::TooLarge)
        );
    }

    /// Stamped identity and the socket request that mints it. They live here
    /// rather than in `lib.rs`'s test module only because that module is 3000
    /// lines away from anything related.
    #[test]
    fn app_socket_and_surface_origin_round_trip() {
        use crate::{
            msg_app_socket_reply, msg_app_socket_request, msg_surface_origin,
            parse_app_socket_reply, parse_app_socket_request, parse_server_msg,
        };

        let request = msg_app_socket_request(11, "legcord", "a1b2");
        assert_eq!(
            parse_app_socket_request(&request),
            Some((11, "legcord", "a1b2"))
        );

        let reply = msg_app_socket_reply(11, crate::STATUS_OK, "blit-app-legcord-a1b2");
        assert_eq!(
            parse_app_socket_reply(&reply),
            Some((11, crate::STATUS_OK, "blit-app-legcord-a1b2"))
        );
        // A refusal carries an empty name, and must still parse.
        let refused = msg_app_socket_reply(11, crate::STATUS_INVALID, "");
        assert_eq!(
            parse_app_socket_reply(&refused),
            Some((11, crate::STATUS_INVALID, ""))
        );

        let origin = msg_surface_origin(7, "blit", "legcord", "a1b2");
        match parse_server_msg(&origin) {
            Some(crate::ServerMsg::SurfaceOrigin {
                surface_id,
                sandbox_engine,
                app_id,
                instance_id,
            }) => {
                assert_eq!(
                    (surface_id, sandbox_engine, app_id, instance_id),
                    (7, "blit", "legcord", "a1b2")
                );
            }
            _ => panic!("SURFACE_ORIGIN did not decode as SurfaceOrigin"),
        }

        // Truncation must fail the parse rather than read a field as empty —
        // an empty app_id would silently attribute a window to nobody.
        for cut in 1..origin.len() {
            let partial = &origin[..cut];
            assert!(
                !matches!(
                    parse_server_msg(partial),
                    Some(crate::ServerMsg::SurfaceOrigin { .. })
                ),
                "prefix of length {cut} must not decode as SurfaceOrigin"
            );
        }
        let mut trailing = origin;
        trailing.push(0);
        assert!(!matches!(
            parse_server_msg(&trailing),
            Some(crate::ServerMsg::SurfaceOrigin { .. })
        ));
    }

    /// A duplicate key on the wire would silently drop one value.
    #[test]
    fn a_duplicate_key_is_malformed() {
        let mut msg = vec![S2C_ENV];
        msg.extend_from_slice(&1u16.to_le_bytes());
        msg.push(crate::STATUS_OK);
        msg.extend_from_slice(&2u16.to_le_bytes());
        for value in [b"one", b"two"] {
            msg.extend_from_slice(&1u16.to_le_bytes());
            msg.push(b'K');
            msg.extend_from_slice(&3u32.to_le_bytes());
            msg.extend_from_slice(value);
        }
        assert_eq!(parse_env(&msg), Err(EnvCodecError::Invalid));
    }
}
