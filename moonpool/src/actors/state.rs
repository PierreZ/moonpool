//! Actor state persistence: durable storage for virtual actor state.
//!
//! The state store is the actor system's persistence layer — given an actor
//! identity, it reads and writes serialized state with ETag-based optimistic
//! concurrency control.
//!
//! # Design
//!
//! - `ActorStateStore` is a trait so implementations can range from a simple
//!   in-memory map (testing/simulation) to a real database backend.
//! - ETags prevent lost updates: a write succeeds only if the stored ETag
//!   matches the expected ETag. On mismatch, the caller gets `ETagMismatch`.
//! - State is stored as opaque bytes — serialization is handled by
//!   [`PersistentState<T>`](super::PersistentState).
//!
//! # Orleans Reference
//!
//! This corresponds to Orleans' `IGrainStorage` provider interface, with
//! `StoredState` mapping to `GrainState<T>` and ETag-based optimistic
//! concurrency matching Orleans' write model.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;

/// Errors from state store operations.
#[derive(Debug, thiserror::Error)]
pub enum ActorStateError {
    /// Write failed because the stored ETag does not match the expected ETag.
    #[error("ETag mismatch: expected {expected:?}, found {actual:?}")]
    ETagMismatch {
        /// The ETag the caller expected.
        expected: String,
        /// The ETag currently in the store.
        actual: String,
    },

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// Generic store error.
    #[error("store error: {0}")]
    StoreError(String),
}

/// A stored actor state entry: serialized data plus an ETag.
#[derive(Debug, Clone)]
pub struct StoredState {
    /// Serialized state bytes.
    pub data: Vec<u8>,
    /// Optimistic concurrency token.
    pub etag: String,
}

/// Trait for durable actor state storage.
///
/// Implementations provide read/write/clear operations for actor state,
/// keyed by actor type name and actor identity string. ETags enable
/// optimistic concurrency control.
///
/// # Single-core
///
/// No Send bounds — matches moonpool's single-core execution model.
#[async_trait::async_trait(?Send)]
pub trait ActorStateStore: fmt::Debug {
    /// Read the stored state for an actor.
    ///
    /// Returns `Ok(None)` if no state has been written for this actor.
    async fn read_state(
        &self,
        actor_type: &str,
        actor_id: &str,
    ) -> Result<Option<StoredState>, ActorStateError>;

    /// Write state for an actor.
    ///
    /// If `expected_etag` is `Some`, the write succeeds only if the currently
    /// stored ETag matches. If `None`, the write is unconditional (first write).
    ///
    /// Returns the new ETag on success.
    async fn write_state(
        &self,
        actor_type: &str,
        actor_id: &str,
        data: Vec<u8>,
        expected_etag: Option<&str>,
    ) -> Result<String, ActorStateError>;

    /// Clear (delete) the stored state for an actor.
    ///
    /// If `expected_etag` is `Some`, the clear succeeds only if the currently
    /// stored ETag matches. If `None`, the clear is unconditional.
    async fn clear_state(
        &self,
        actor_type: &str,
        actor_id: &str,
        expected_etag: Option<&str>,
    ) -> Result<(), ActorStateError>;
}

/// In-memory state store for testing and simulation.
///
/// All state lives in a `HashMap`. ETags are monotonically increasing
/// counter values. No persistence across process restarts.
#[derive(Debug)]
pub struct InMemoryStateStore {
    entries: RefCell<HashMap<(String, String), StoredState>>,
    counter: Cell<u64>,
}

impl InMemoryStateStore {
    /// Create a new empty in-memory state store.
    pub fn new() -> Self {
        Self {
            entries: RefCell::new(HashMap::new()),
            counter: Cell::new(0),
        }
    }

    fn next_etag(&self) -> String {
        let val = self.counter.get() + 1;
        self.counter.set(val);
        val.to_string()
    }
}

impl Default for InMemoryStateStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait(?Send)]
impl ActorStateStore for InMemoryStateStore {
    async fn read_state(
        &self,
        actor_type: &str,
        actor_id: &str,
    ) -> Result<Option<StoredState>, ActorStateError> {
        let key = (actor_type.to_string(), actor_id.to_string());
        Ok(self.entries.borrow().get(&key).cloned())
    }

    async fn write_state(
        &self,
        actor_type: &str,
        actor_id: &str,
        data: Vec<u8>,
        expected_etag: Option<&str>,
    ) -> Result<String, ActorStateError> {
        let key = (actor_type.to_string(), actor_id.to_string());
        let mut entries = self.entries.borrow_mut();

        // Check ETag if expected
        if let Some(expected) = expected_etag {
            match entries.get(&key) {
                Some(existing) if existing.etag != expected => {
                    moonpool_sim::assert_sometimes!(true, "etag_conflict");
                    return Err(ActorStateError::ETagMismatch {
                        expected: expected.to_string(),
                        actual: existing.etag.clone(),
                    });
                }
                None => {
                    moonpool_sim::assert_sometimes!(true, "etag_conflict");
                    return Err(ActorStateError::ETagMismatch {
                        expected: expected.to_string(),
                        actual: String::new(),
                    });
                }
                _ => {}
            }
        }

        let new_etag = self.next_etag();
        entries.insert(
            key,
            StoredState {
                data,
                etag: new_etag.clone(),
            },
        );
        Ok(new_etag)
    }

    async fn clear_state(
        &self,
        actor_type: &str,
        actor_id: &str,
        expected_etag: Option<&str>,
    ) -> Result<(), ActorStateError> {
        let key = (actor_type.to_string(), actor_id.to_string());
        let mut entries = self.entries.borrow_mut();

        // Check ETag if expected
        if let Some(expected) = expected_etag {
            match entries.get(&key) {
                Some(existing) if existing.etag != expected => {
                    return Err(ActorStateError::ETagMismatch {
                        expected: expected.to_string(),
                        actual: existing.etag.clone(),
                    });
                }
                None => {
                    // Nothing to clear, but ETag was expected — mismatch
                    return Err(ActorStateError::ETagMismatch {
                        expected: expected.to_string(),
                        actual: String::new(),
                    });
                }
                _ => {}
            }
        }

        entries.remove(&key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_empty() {
        let store = InMemoryStateStore::new();
        let result = store.read_state("Counter", "alice").await;
        assert!(result.is_ok());
        assert!(result.expect("read should succeed").is_none());
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let store = InMemoryStateStore::new();
        let data = vec![1, 2, 3];

        let etag = store
            .write_state("Counter", "alice", data.clone(), None)
            .await
            .expect("write should succeed");
        assert!(!etag.is_empty());

        let stored = store
            .read_state("Counter", "alice")
            .await
            .expect("read should succeed")
            .expect("state should exist");
        assert_eq!(stored.data, data);
        assert_eq!(stored.etag, etag);
    }

    #[tokio::test]
    async fn test_write_etag_match() {
        let store = InMemoryStateStore::new();

        let etag1 = store
            .write_state("Counter", "alice", vec![1], None)
            .await
            .expect("first write");

        let etag2 = store
            .write_state("Counter", "alice", vec![2], Some(&etag1))
            .await
            .expect("second write with matching etag");

        assert_ne!(etag1, etag2);

        let stored = store
            .read_state("Counter", "alice")
            .await
            .expect("read")
            .expect("exists");
        assert_eq!(stored.data, vec![2]);
        assert_eq!(stored.etag, etag2);
    }

    #[tokio::test]
    async fn test_write_etag_mismatch() {
        let store = InMemoryStateStore::new();

        let _etag = store
            .write_state("Counter", "alice", vec![1], None)
            .await
            .expect("first write");

        let result = store
            .write_state("Counter", "alice", vec![2], Some("wrong-etag"))
            .await;
        assert!(matches!(result, Err(ActorStateError::ETagMismatch { .. })));

        // Original data unchanged
        let stored = store
            .read_state("Counter", "alice")
            .await
            .expect("read")
            .expect("exists");
        assert_eq!(stored.data, vec![1]);
    }

    #[tokio::test]
    async fn test_clear() {
        let store = InMemoryStateStore::new();

        store
            .write_state("Counter", "alice", vec![1], None)
            .await
            .expect("write");

        store
            .clear_state("Counter", "alice", None)
            .await
            .expect("clear should succeed");

        let result = store.read_state("Counter", "alice").await.expect("read");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_clear_etag_mismatch() {
        let store = InMemoryStateStore::new();

        store
            .write_state("Counter", "alice", vec![1], None)
            .await
            .expect("write");

        let result = store
            .clear_state("Counter", "alice", Some("wrong-etag"))
            .await;
        assert!(matches!(result, Err(ActorStateError::ETagMismatch { .. })));

        // Data still exists
        let stored = store.read_state("Counter", "alice").await.expect("read");
        assert!(stored.is_some());
    }

    #[tokio::test]
    async fn test_independent_actors() {
        let store = InMemoryStateStore::new();

        store
            .write_state("Counter", "alice", vec![10], None)
            .await
            .expect("write alice");
        store
            .write_state("Counter", "bob", vec![20], None)
            .await
            .expect("write bob");

        let alice = store
            .read_state("Counter", "alice")
            .await
            .expect("read alice")
            .expect("alice exists");
        let bob = store
            .read_state("Counter", "bob")
            .await
            .expect("read bob")
            .expect("bob exists");

        assert_eq!(alice.data, vec![10]);
        assert_eq!(bob.data, vec![20]);
    }

    #[tokio::test]
    async fn test_different_actor_types() {
        let store = InMemoryStateStore::new();

        store
            .write_state("Counter", "alice", vec![1], None)
            .await
            .expect("write Counter/alice");
        store
            .write_state("BankAccount", "alice", vec![2], None)
            .await
            .expect("write BankAccount/alice");

        let counter = store
            .read_state("Counter", "alice")
            .await
            .expect("read")
            .expect("exists");
        let bank = store
            .read_state("BankAccount", "alice")
            .await
            .expect("read")
            .expect("exists");

        assert_eq!(counter.data, vec![1]);
        assert_eq!(bank.data, vec![2]);
    }
}
