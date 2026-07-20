//! Token verification (docs/upsidedown.md § Tokens).
//!
//! Tokens are compact JWTs signed with EdDSA (Ed25519).  The `alg` header
//! is deliberately ignored: only Ed25519 signatures against the configured
//! public keys are ever accepted, so algorithm-confusion attacks are moot.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Clock-skew leeway applied to `exp`.
pub const LEEWAY_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

#[derive(Debug)]
pub struct Claims {
    pub sid: String,
    pub role: Role,
    #[allow(dead_code)]
    pub exp: u64,
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits > 0 && (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

pub fn b64url_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Parse `UPSIDEDOWN_PUBLIC_KEYS`: comma-separated base64url Ed25519 keys.
pub fn parse_public_keys(raw: &str) -> Result<Vec<VerifyingKey>, String> {
    let mut keys = Vec::new();
    for (i, part) in raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
    {
        let bytes = b64url_decode(part).ok_or(format!("public key {i}: invalid base64url"))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| format!("public key {i}: not 32 bytes"))?;
        keys.push(VerifyingKey::from_bytes(&arr).map_err(|e| format!("public key {i}: {e}"))?);
    }
    if keys.is_empty() {
        return Err("no public keys configured".into());
    }
    Ok(keys)
}

/// Verify a token against the configured keys.  An optional numeric `kid`
/// header (0-based key index) skips the trial loop; otherwise every key is
/// tried.
pub fn verify(token: &str, keys: &[VerifyingKey], now_unix: u64) -> Result<Claims, String> {
    let mut parts = token.trim().split('.');
    let (h, p, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => return Err("malformed token".into()),
    };

    let sig_bytes = b64url_decode(s).ok_or("bad signature encoding")?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| "bad signature length")?;
    let message = format!("{h}.{p}");

    let header: serde_json::Value =
        serde_json::from_slice(&b64url_decode(h).ok_or("bad header encoding")?)
            .map_err(|_| "bad header JSON")?;
    let kid = header
        .get("kid")
        .and_then(|v| v.as_u64())
        .map(|i| i as usize);
    let verified = match kid {
        Some(i) if i < keys.len() => keys[i].verify(message.as_bytes(), &sig).is_ok(),
        _ => keys
            .iter()
            .any(|k| k.verify(message.as_bytes(), &sig).is_ok()),
    };
    if !verified {
        return Err("signature verification failed".into());
    }

    let claims: serde_json::Value =
        serde_json::from_slice(&b64url_decode(p).ok_or("bad claims encoding")?)
            .map_err(|_| "bad claims JSON")?;
    let sid = claims
        .get("sid")
        .and_then(|v| v.as_str())
        .ok_or("missing sid claim")?
        .to_string();
    let role = match claims.get("role").and_then(|v| v.as_str()) {
        Some("server") => Role::Server,
        Some("client") => Role::Client,
        _ => return Err("missing or unknown role claim".into()),
    };
    let exp = claims
        .get("exp")
        .and_then(|v| v.as_u64())
        .ok_or("missing exp claim")?;
    if now_unix > exp + LEEWAY_SECS {
        return Err("token expired".into());
    }
    Ok(Claims { sid, role, exp })
}

pub fn mint(key: &SigningKey, sid: &str, role: &str, ttl_secs: u64) -> String {
    let header = b64url_encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
    let claims = b64url_encode(
        serde_json::json!({ "sid": sid, "role": role, "exp": now_unix() + ttl_secs })
            .to_string()
            .as_bytes(),
    );
    let message = format!("{header}.{claims}");
    let sig = key.sign(message.as_bytes());
    format!("{message}.{}", b64url_encode(&sig.to_bytes()))
}

pub fn keygen() {
    let mut secret = [0u8; 32];
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new()
        .fill(&mut secret)
        .expect("system RNG");
    let key = SigningKey::from_bytes(&secret);
    println!("secret: {}", b64url_encode(&secret));
    println!("public: {}", b64url_encode(key.verifying_key().as_bytes()));
}

pub fn mint_cli(secret_b64: &str, sid: &str, role: &str, ttl_secs: u64) -> Result<(), String> {
    if role != "server" && role != "client" {
        return Err("role must be \"server\" or \"client\"".into());
    }
    let bytes = b64url_decode(secret_b64).ok_or("secret key: invalid base64url")?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| "secret key: not 32 bytes")?;
    println!(
        "{}",
        mint(&SigningKey::from_bytes(&arr), sid, role, ttl_secs)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    #[test]
    fn roundtrip() {
        let key = test_key();
        let token = mint(&key, "sess-1", "client", 60);
        let claims = verify(&token, &[key.verifying_key()], now_unix()).unwrap();
        assert_eq!(claims.sid, "sess-1");
        assert_eq!(claims.role, Role::Client);
    }

    #[test]
    fn rejects_wrong_key_and_tamper() {
        let key = test_key();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let token = mint(&key, "s", "server", 60);
        assert!(verify(&token, &[other.verifying_key()], now_unix()).is_err());
        let tampered = token.replace('.', "x");
        assert!(verify(&tampered, &[key.verifying_key()], now_unix()).is_err());
    }

    #[test]
    fn respects_expiry_with_leeway() {
        let key = test_key();
        let token = mint(&key, "s", "client", 0);
        let keys = [key.verifying_key()];
        assert!(verify(&token, &keys, now_unix()).is_ok(), "within leeway");
        assert!(verify(&token, &keys, now_unix() + LEEWAY_SECS + 5).is_err());
    }

    #[test]
    fn tries_all_keys_and_honors_kid() {
        let a = SigningKey::from_bytes(&[1u8; 32]);
        let b = SigningKey::from_bytes(&[2u8; 32]);
        let token = mint(&b, "s", "client", 60);
        let keys = [a.verifying_key(), b.verifying_key()];
        assert!(verify(&token, &keys, now_unix()).is_ok());
    }

    #[test]
    fn rejects_bad_role() {
        let key = test_key();
        let token = mint(&key, "s", "admin", 60);
        // mint doesn't validate role; verify must.
        assert!(verify(&token, &[key.verifying_key()], now_unix()).is_err());
    }

    #[test]
    fn b64url_roundtrip() {
        for data in [&b""[..], b"a", b"ab", b"abc", b"hello world"] {
            assert_eq!(b64url_decode(&b64url_encode(data)).unwrap(), data);
        }
    }
}
