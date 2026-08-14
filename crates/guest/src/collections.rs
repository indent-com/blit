//! Entropy-keyed collections for attacker-controlled keys.

use core::hash::BuildHasher;

use siphasher::sip::SipHasher13;

/// A SipHash-1-3 builder keyed from `blit_v1.random`.
#[derive(Clone, Debug)]
pub struct RandomState {
    key0: u64,
    key1: u64,
}

impl RandomState {
    /// Obtain fresh collection keys from the host entropy source.
    pub fn try_new() -> Result<Self, crate::host::Error> {
        let mut key = [0u8; 16];
        crate::host::random(&mut key)?;
        Ok(Self {
            key0: u64::from_le_bytes(key[..8].try_into().expect("fixed key half")),
            key1: u64::from_le_bytes(key[8..].try_into().expect("fixed key half")),
        })
    }

    /// Obtain fresh collection keys, trapping/panicking if the host ABI is
    /// unavailable. A Wasmi entropy failure already terminates the attempt.
    pub fn new() -> Self {
        Self::try_new().expect("blit_v1.random rejected a fixed-size key")
    }
}

impl Default for RandomState {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildHasher for RandomState {
    type Hasher = SipHasher13;

    fn build_hasher(&self) -> Self::Hasher {
        SipHasher13::new_with_keys(self.key0, self.key1)
    }
}

/// Entropy-keyed hash map for keys controlled by an untrusted peer.
pub type HashMap<K, V> = hashbrown::HashMap<K, V, RandomState>;
/// Entropy-keyed hash set for keys controlled by an untrusted peer.
pub type HashSet<K> = hashbrown::HashSet<K, RandomState>;

/// Construct an empty entropy-keyed [`HashMap`].
pub fn hash_map<K, V>() -> HashMap<K, V> {
    HashMap::with_hasher(RandomState::new())
}

/// Construct an empty entropy-keyed [`HashSet`].
pub fn hash_set<K>() -> HashSet<K> {
    HashSet::with_hasher(RandomState::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_host;

    struct Entropy(u8);

    impl native_host::Host for Entropy {
        fn send(&mut self, _: &[u8]) -> i32 {
            unreachable!()
        }

        fn recv(&mut self, _: &mut [u8]) -> i32 {
            unreachable!()
        }

        fn wait(&mut self, _: i64) -> i32 {
            unreachable!()
        }

        fn clock(&mut self, _: i32) -> i64 {
            unreachable!()
        }

        fn random(&mut self, destination: &mut [u8]) {
            for byte in destination {
                *byte = self.0;
                self.0 = self.0.wrapping_add(1);
            }
        }
    }

    #[test]
    fn aliases_use_host_keyed_siphash() {
        let _guard = native_host::install(Entropy(1));
        let mut map = hash_map();
        map.insert("untrusted", 42);
        assert_eq!(map.get("untrusted"), Some(&42));

        let mut set = hash_set();
        set.insert("peer");
        assert!(set.contains("peer"));
    }
}
