//! Environment configuration shared by the control plane and workers.

use crate::store::Store;

#[derive(Clone)]
pub struct Env {
    pub store: Store,
    pub keys: Vec<ed25519_dalek::VerifyingKey>,
    pub seal_key: [u8; 32],
    /// Public hostname consumers and uplinks connect to.
    pub host: String,
}

pub async fn load() -> Result<Env, String> {
    let redis_url =
        std::env::var("REDIS_URL").map_err(|_| "REDIS_URL is not set (use memory:// for dev)")?;
    let store = Store::open(&redis_url).await?;

    let keys_raw =
        std::env::var("UPSIDEDOWN_PUBLIC_KEYS").map_err(|_| "UPSIDEDOWN_PUBLIC_KEYS is not set")?;
    let keys = crate::jwt::parse_public_keys(&keys_raw)?;

    let seal_raw =
        std::env::var("UPSIDEDOWN_SEAL_KEY").map_err(|_| "UPSIDEDOWN_SEAL_KEY is not set")?;
    let seal_bytes = crate::jwt::b64url_decode(seal_raw.trim())
        .ok_or("UPSIDEDOWN_SEAL_KEY: invalid base64url")?;
    let seal_key: [u8; 32] = seal_bytes
        .try_into()
        .map_err(|_| "UPSIDEDOWN_SEAL_KEY: not 32 bytes")?;

    let host = std::env::var("UPSIDEDOWN_HOST").unwrap_or_else(|_| "usd.blit.sh".to_string());

    Ok(Env {
        store,
        keys,
        seal_key,
        host,
    })
}
