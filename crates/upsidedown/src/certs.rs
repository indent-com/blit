//! Certificate handling (docs/upsidedown.md § Certificates).
//!
//! The control plane drives ACME and stores the result — sealed — in the
//! store; workers unseal it and terminate TLS themselves.  Without an ACME
//! domain configured (development), the control plane writes a self-signed
//! bundle instead and `/pool` pins it for uplinks.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::store::Store;

pub const CERT_KEY: &str = "acme:cert";
pub const ACCOUNT_KEY: &str = "acme:account";

// ---------------------------------------------------------------------------
// Sealing: ChaCha20-Poly1305 under UPSIDEDOWN_SEAL_KEY, so Redis alone
// yields no TLS key material.  Format: base64url(nonce12 || ciphertext).
// ---------------------------------------------------------------------------

pub fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<String, String> {
    use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
    use ring::rand::SecureRandom;

    let unbound =
        UnboundKey::new(&CHACHA20_POLY1305, key).map_err(|_| "bad seal key".to_string())?;
    let sealing = LessSafeKey::new(unbound);
    let mut nonce = [0u8; NONCE_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| "rng failure".to_string())?;
    let mut in_out = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::empty(),
            &mut in_out,
        )
        .map_err(|_| "seal failure".to_string())?;
    let mut framed = nonce.to_vec();
    framed.extend_from_slice(&in_out);
    Ok(crate::jwt::b64url_encode(&framed))
}

pub fn open_sealed(key: &[u8; 32], sealed: &str) -> Result<Vec<u8>, String> {
    use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};

    let framed = crate::jwt::b64url_decode(sealed).ok_or("sealed value: invalid base64url")?;
    if framed.len() < NONCE_LEN {
        return Err("sealed value too short".into());
    }
    let (nonce, ct) = framed.split_at(NONCE_LEN);
    let unbound =
        UnboundKey::new(&CHACHA20_POLY1305, key).map_err(|_| "bad seal key".to_string())?;
    let opening = LessSafeKey::new(unbound);
    let mut in_out = ct.to_vec();
    let plain = opening
        .open_in_place(
            Nonce::assume_unique_for_key(nonce.try_into().unwrap()),
            Aad::empty(),
            &mut in_out,
        )
        .map_err(|_| "unseal failure (wrong UPSIDEDOWN_SEAL_KEY?)".to_string())?;
    Ok(plain.to_vec())
}

// ---------------------------------------------------------------------------
// PEM bundles: private key first, then the certificate chain (the format
// rustls-acme stores).
// ---------------------------------------------------------------------------

pub type Bundle = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

pub fn parse_bundle(pem: &[u8]) -> Result<Bundle, String> {
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("bad certificate PEM: {e}"))?;
    let key = rustls_pemfile::private_key(&mut &pem[..])
        .map_err(|e| format!("bad key PEM: {e}"))?
        .ok_or("no private key in bundle")?;
    if certs.is_empty() {
        return Err("no certificates in bundle".into());
    }
    Ok((certs, key))
}

/// SHA-256 of the leaf certificate (for dev-mode `#sha256=` pins).
pub fn leaf_hash(bundle: &Bundle) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bundle.0[0].as_ref())
        .as_ref()
        .to_vec()
}

/// Generate a development self-signed bundle for `host`.
pub fn dev_bundle_pem(host: &str) -> Result<String, String> {
    use rcgen::{CertificateParams, KeyPair};
    let params =
        CertificateParams::new(vec![host.to_string()]).map_err(|e| format!("rcgen: {e}"))?;
    let key_pair = KeyPair::generate().map_err(|e| format!("rcgen: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("rcgen: {e}"))?;
    Ok(format!("{}{}", key_pair.serialize_pem(), cert.pem()))
}

/// Load and unseal the current certificate bundle, if present.
pub async fn load_bundle(store: &Store, seal_key: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
    match store.get(CERT_KEY).await? {
        Some(sealed) => Ok(Some(open_sealed(seal_key, &sealed)?)),
        None => Ok(None),
    }
}

/// rustls ServerConfig for the worker's WebSocket (TCP) listener.
pub fn tls_config(bundle: &Bundle) -> Result<Arc<rustls::ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .map_err(|e| format!("TLS config: {e}"))?
        .with_no_client_auth()
        .with_single_cert(bundle.0.clone(), bundle.1.clone_key())
        .map_err(|e| format!("TLS config: {e}"))?;
    Ok(Arc::new(config))
}

/// quinn ServerConfig (WebTransport ALPN) for the worker's UDP listener.
pub fn quic_config(bundle: &Bundle) -> Result<web_transport_quinn::quinn::ServerConfig, String> {
    use web_transport_quinn as wt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("QUIC TLS config: {e}"))?
        .with_no_client_auth()
        .with_single_cert(bundle.0.clone(), bundle.1.clone_key())
        .map_err(|e| format!("QUIC TLS config: {e}"))?;
    tls.alpn_protocols = vec![wt::ALPN.as_bytes().to_vec()];
    let quic: wt::quinn::crypto::rustls::QuicServerConfig = tls
        .try_into()
        .map_err(|e| format!("QUIC TLS config: {e}"))?;
    Ok(wt::quinn::ServerConfig::with_crypto(Arc::new(quic)))
}

// ---------------------------------------------------------------------------
// rustls-acme cache backed by the sealed store keys.
// ---------------------------------------------------------------------------

pub struct SealedCache {
    pub store: Store,
    pub seal_key: [u8; 32],
}

#[async_trait::async_trait]
impl rustls_acme::CertCache for SealedCache {
    type EC = String;
    async fn load_cert(
        &self,
        _domains: &[String],
        _directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EC> {
        load_bundle(&self.store, &self.seal_key).await
    }
    async fn store_cert(
        &self,
        _domains: &[String],
        _directory_url: &str,
        cert: &[u8],
    ) -> Result<(), Self::EC> {
        let sealed = seal(&self.seal_key, cert)?;
        self.store.set(CERT_KEY, &sealed).await
    }
}

#[async_trait::async_trait]
impl rustls_acme::AccountCache for SealedCache {
    type EA = String;
    async fn load_account(
        &self,
        _contact: &[String],
        _directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EA> {
        match self.store.get(ACCOUNT_KEY).await? {
            Some(sealed) => Ok(Some(open_sealed(&self.seal_key, &sealed)?)),
            None => Ok(None),
        }
    }
    async fn store_account(
        &self,
        _contact: &[String],
        _directory_url: &str,
        account: &[u8],
    ) -> Result<(), Self::EA> {
        let sealed = seal(&self.seal_key, account)?;
        self.store.set(ACCOUNT_KEY, &sealed).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_roundtrip() {
        let key = [3u8; 32];
        let sealed = seal(&key, b"secret material").unwrap();
        assert_eq!(open_sealed(&key, &sealed).unwrap(), b"secret material");
        assert!(open_sealed(&[4u8; 32], &sealed).is_err(), "wrong key fails");
    }

    #[test]
    fn dev_bundle_parses() {
        let pem = dev_bundle_pem("upsidedown.test").unwrap();
        let bundle = parse_bundle(pem.as_bytes()).unwrap();
        assert_eq!(bundle.0.len(), 1);
        assert_eq!(leaf_hash(&bundle).len(), 32);
        assert!(tls_config(&bundle).is_ok());
        assert!(quic_config(&bundle).is_ok());
    }
}
