//! Passphrase hashing and verification for gateway/browser authentication.
//!
//! `BLIT_PASSPHRASE` can be either a legacy plaintext passphrase or an argon2
//! PHC string (salt and parameters embedded). Verification transparently uses
//! argon2 for `$argon2...` values; browser clients still send the plaintext
//! passphrase.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

/// PHC strings produced by the argon2 crate always start with this marker
/// (covers `$argon2id$`, `$argon2i$`, and `$argon2d$`).
const PHC_PREFIX: &str = "$argon2";

/// Configured browser-auth passphrase for a gateway/webserver endpoint.
#[derive(Clone, Debug)]
pub enum AuthPassphrase {
    Plaintext(String),
    Argon2(String),
}

impl AuthPassphrase {
    /// Build a passphrase verifier from the raw configured value.
    ///
    /// Argon2 PHC strings are detected by prefix; anything else is treated as
    /// legacy plaintext.
    pub fn new(value: String) -> Self {
        if is_hashed(&value) {
            Self::Argon2(value)
        } else {
            Self::Plaintext(value)
        }
    }

    /// Build from `BLIT_PASSPHRASE`.
    pub fn from_env_value(value: String) -> Self {
        Self::new(value)
    }

    /// Build a plaintext verifier. Useful for the CLI's random local browser
    /// token, which is not stored and does not need hashing.
    pub fn plaintext(value: impl Into<String>) -> Self {
        Self::Plaintext(value.into())
    }

    /// Build an argon2 verifier from an existing PHC hash.
    pub fn argon2(value: impl Into<String>) -> Self {
        Self::Argon2(value.into())
    }

    /// Verify a client-provided plaintext passphrase against this configured
    /// verifier.
    pub fn verify(&self, provided: &str) -> bool {
        match self {
            Self::Plaintext(expected) => constant_time_eq(provided.as_bytes(), expected.as_bytes()),
            Self::Argon2(hash) => verify_argon2(provided, hash),
        }
    }

    pub fn is_argon2(&self) -> bool {
        matches!(self, Self::Argon2(_))
    }
}

/// Hash `passphrase` with argon2id and a fresh random salt, returning a PHC
/// string suitable for `BLIT_PASSPHRASE`.
pub fn hash(passphrase: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(passphrase.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("cannot hash passphrase: {e}"))
}

/// Returns true when `stored` looks like an argon2 PHC hash rather than a
/// legacy plaintext passphrase.
pub fn is_hashed(stored: &str) -> bool {
    stored.starts_with(PHC_PREFIX)
}

/// Verify a client-`provided` passphrase against the `stored` value.
///
/// Argon2 PHC hashes are verified with the embedded salt/parameters; anything
/// else is treated as legacy plaintext and compared in constant time.
pub fn verify(provided: &str, stored: &str) -> bool {
    if is_hashed(stored) {
        verify_argon2(provided, stored)
    } else {
        constant_time_eq(provided.as_bytes(), stored.as_bytes())
    }
}

fn verify_argon2(provided: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(provided.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Compare two byte strings without letting the time taken reveal how much
/// of `b` was matched, or how long `b` is.
///
/// Comparing the inputs directly cannot do this: a byte loop has to bail
/// when the lengths differ, and that early return is an oracle for the
/// length of the stored secret. Hash both sides to a fixed 32 bytes first,
/// so the comparison is the same work on every call regardless of input.
///
/// Hashing `a` costs time proportional to its length, but the caller
/// supplied `a` and already knows it. Hashing `b` is the same cost on every
/// attempt, so it reveals nothing across guesses.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let a = blake3::hash(a);
    let b = blake3::hash(b);
    let mut diff = 0u8;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_phc_strings_and_verify() {
        let stored = hash("hunter2").unwrap();
        assert!(is_hashed(&stored));
        assert!(stored.starts_with("$argon2id$"));
        assert!(verify("hunter2", &stored));
        assert!(!verify("hunter3", &stored));

        let auth = AuthPassphrase::new(stored);
        assert!(auth.verify("hunter2"));
        assert!(!auth.verify("hunter3"));
        assert!(auth.is_argon2());
    }

    #[test]
    fn each_hash_uses_a_fresh_salt() {
        assert_ne!(hash("same").unwrap(), hash("same").unwrap());
    }

    #[test]
    fn phc_hashes_are_auto_detected() {
        let stored = hash("hunter2").unwrap();
        let auth = AuthPassphrase::new(stored.clone());
        assert!(auth.is_argon2());
        assert!(auth.verify("hunter2"));
        assert!(!auth.verify(&stored));
    }

    #[test]
    fn legacy_plaintext_still_verifies() {
        assert!(!is_hashed("blit-secret"));
        assert!(verify("blit-secret", "blit-secret"));
        assert!(!verify("blit-secret", "other"));

        let auth = AuthPassphrase::plaintext("blit-secret");
        assert!(auth.verify("blit-secret"));
        assert!(!auth.verify("other"));
    }

    #[test]
    fn malformed_hash_rejects() {
        assert!(!verify("x", "$argon2id$not-a-real-hash"));
        let auth = AuthPassphrase::new("$argon2id$not-a-real-hash".into());
        assert!(auth.is_argon2());
        assert!(!auth.verify("x"));
    }

    #[test]
    fn constant_time_eq_cases() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
    }

    /// Length mismatches must go down the same path as content mismatches.
    /// The comparison is over fixed-width digests precisely so a wrong-length
    /// guess is not distinguishable from a wrong-content one — the earlier
    /// version returned before looking at a single byte.
    #[test]
    fn a_wrong_length_guess_is_just_a_wrong_guess() {
        let secret = b"correct horse battery staple";
        for guess in [
            b"".as_slice(),
            b"c",
            b"correct horse battery stapl",
            b"correct horse battery staple!",
            b"correct horse battery staple with a great deal more text after it",
        ] {
            assert!(!constant_time_eq(guess, secret), "{guess:?} must not match");
        }
        assert!(constant_time_eq(secret, secret));
    }
}
