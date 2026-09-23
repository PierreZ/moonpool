//! Verification key sets and their rotation.
//!
//! Rotation replaces the **whole** set, as `FoundationDB`'s
//! `applyPublicKeySet` does after re-reading its JWKS file
//! (`fdbrpc/FlowTransport.cpp`): a key missing from the new set is revoked
//! at once, for new requests and for anything a verifier cached. Every
//! replacement gets a new generation, which is what verification caches
//! compare against.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// One immutable set of verification keys, by key id.
#[derive(Debug)]
pub struct KeySet<K> {
    generation: u64,
    keys: BTreeMap<String, K>,
}

impl<K> KeySet<K> {
    /// The set's generation: bumped by every replacement.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The key with id `kid`.
    #[must_use]
    pub fn get(&self, kid: &str) -> Option<&K> {
        self.keys.get(kid)
    }

    /// How many keys the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set holds no key (every credential is then refused).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The key ids, in order.
    pub fn key_ids(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(String::as_str)
    }
}

/// The current key set, replaceable while verifiers read it. Clones share
/// the set.
#[derive(Debug)]
pub struct RotatingKeys<K> {
    current: Arc<RwLock<Arc<KeySet<K>>>>,
}

impl<K> Clone for RotatingKeys<K> {
    fn clone(&self) -> Self {
        Self {
            current: Arc::clone(&self.current),
        }
    }
}

impl<K> RotatingKeys<K> {
    /// Start with `keys` (id, key) at generation 1.
    #[must_use]
    pub fn new(keys: impl IntoIterator<Item = (String, K)>) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(KeySet {
                generation: 1,
                keys: keys.into_iter().collect(),
            }))),
        }
    }

    /// The current set. Holding it keeps verifying against it; the next
    /// call sees any replacement.
    ///
    /// # Panics
    ///
    /// Only if a thread panicked while replacing the set.
    #[must_use]
    pub fn current(&self) -> Arc<KeySet<K>> {
        Arc::clone(
            &self
                .current
                .read()
                .expect("RwLock poisoned: prior task panicked"),
        )
    }

    /// Replace the whole set with `keys`; returns the new generation. A key
    /// left out is revoked from now on.
    ///
    /// # Panics
    ///
    /// Only if a thread panicked while replacing the set.
    pub fn replace(&self, keys: impl IntoIterator<Item = (String, K)>) -> u64 {
        let mut current = self
            .current
            .write()
            .expect("RwLock poisoned: prior task panicked");
        let generation = current.generation.saturating_add(1);
        *current = Arc::new(KeySet {
            generation,
            keys: keys.into_iter().collect(),
        });
        let count = current.keys.len();
        // Traced after the write lock is released: a subscriber must never
        // run while verifiers are blocked.
        drop(current);
        tracing::info!(
            target: "moonpool_rpc::audit",
            generation,
            keys = count,
            "rpc_verification_keys_replaced"
        );
        generation
    }
}

#[cfg(test)]
mod tests {
    use super::RotatingKeys;

    /// Regression: the replacement was traced while the write lock was held,
    /// so a subscriber (here: its writer) that reads the keys deadlocked.
    #[test]
    fn the_replacement_event_is_emitted_outside_the_lock() {
        let keys = RotatingKeys::new([("a".to_string(), 1)]);
        let observer = keys.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || {
                let _ = observer.current();
                std::io::sink()
            })
            .finish();
        let generation =
            tracing::subscriber::with_default(subscriber, || keys.replace([("b".to_string(), 2)]));
        assert_eq!(generation, 2);
    }

    #[test]
    fn replacement_swaps_the_whole_set_and_bumps_the_generation() {
        let keys = RotatingKeys::new([("a".to_string(), 1), ("b".to_string(), 2)]);
        let shared = keys.clone();
        let before = keys.current();
        assert_eq!(before.generation(), 1);
        assert_eq!(before.get("a"), Some(&1));
        assert_eq!(shared.replace([("c".to_string(), 3)]), 2);
        let after = keys.current();
        assert_eq!(after.get("a"), None, "a key left out is revoked");
        assert_eq!(after.key_ids().collect::<Vec<_>>(), ["c"]);
        assert_eq!(before.get("a"), Some(&1), "a held snapshot is immutable");
        assert!(!after.is_empty());
        assert_eq!(after.len(), 1);
    }
}
