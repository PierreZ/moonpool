//! Typed persistent state wrapper for virtual actors.
//!
//! `PersistentState<T>` provides a typed, cached interface over the raw
//! [`ActorStateStore`]. It handles serialization, ETag tracking, and
//! provides ergonomic `state()` / `state_mut()` accessors.
//!
//! # Usage
//!
//! Actors create a `PersistentState<T>` in their `on_activate` hook
//! and store it as a field:
//!
//! ```rust,ignore
//! #[derive(Default)]
//! struct BankAccount {
//!     state: Option<PersistentState<BankAccountData>>,
//! }
//!
//! impl ActorHandler for BankAccount {
//!     async fn on_activate(&mut self, ctx: &ActorContext<impl Providers>) -> Result<(), ActorError> {
//!         let store = ctx.state_store().expect("state store configured");
//!         self.state = Some(PersistentState::load(store.clone(), "BankAccount", &ctx.id.identity).await?);
//!         Ok(())
//!     }
//! }
//! ```
//!
//! # Orleans Reference
//!
//! This corresponds to Orleans' `IPersistentState<T>` / `StateStorageBridge<T>`.

use std::rc::Rc;

use moonpool_sim::assert_sometimes;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::state::{ActorStateError, ActorStateStore};

/// Typed persistent state for a virtual actor.
///
/// Wraps an [`ActorStateStore`] with typed access, ETag tracking, and
/// serialization via `serde_json`. Created via [`PersistentState::load`]
/// during actor activation.
pub struct PersistentState<T> {
    value: T,
    etag: Option<String>,
    record_exists: bool,
    store: Rc<dyn ActorStateStore>,
    actor_type_name: String,
    actor_identity: String,
}

impl<T: Serialize + DeserializeOwned + Default> PersistentState<T> {
    /// Load persistent state from the store.
    ///
    /// Reads existing state and deserializes it, or creates `T::default()`
    /// if no state exists yet. Call this in `on_activate`.
    pub async fn load(
        store: Rc<dyn ActorStateStore>,
        actor_type_name: &str,
        actor_identity: &str,
    ) -> Result<Self, ActorStateError> {
        let stored = store.read_state(actor_type_name, actor_identity).await?;

        match stored {
            Some(entry) => {
                assert_sometimes!(true, "persistent_state_loaded");
                let value: T = serde_json::from_slice(&entry.data)
                    .map_err(|e| ActorStateError::SerializationError(e.to_string()))?;
                Ok(Self {
                    value,
                    etag: Some(entry.etag),
                    record_exists: true,
                    store,
                    actor_type_name: actor_type_name.to_string(),
                    actor_identity: actor_identity.to_string(),
                })
            }
            None => Ok(Self {
                value: T::default(),
                etag: None,
                record_exists: false,
                store,
                actor_type_name: actor_type_name.to_string(),
                actor_identity: actor_identity.to_string(),
            }),
        }
    }

    /// Get a reference to the current state.
    pub fn state(&self) -> &T {
        &self.value
    }

    /// Get a mutable reference to the current state.
    ///
    /// Changes are only persisted when [`write_state`](Self::write_state) is called.
    pub fn state_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Whether this state has been written to the store at least once.
    pub fn record_exists(&self) -> bool {
        self.record_exists
    }

    /// The current ETag, or `None` if the state has never been written.
    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// Persist the current state to the store.
    ///
    /// Serializes `T`, writes to the store with the current ETag for
    /// optimistic concurrency, and updates the local ETag on success.
    pub async fn write_state(&mut self) -> Result<(), ActorStateError> {
        let data = serde_json::to_vec(&self.value)
            .map_err(|e| ActorStateError::SerializationError(e.to_string()))?;

        let new_etag = self
            .store
            .write_state(
                &self.actor_type_name,
                &self.actor_identity,
                data,
                self.etag.as_deref(),
            )
            .await?;

        self.etag = Some(new_etag);
        self.record_exists = true;
        assert_sometimes!(true, "persistent_state_written");
        Ok(())
    }

    /// Re-read state from the store, replacing the local value.
    pub async fn read_state(&mut self) -> Result<(), ActorStateError> {
        let stored = self
            .store
            .read_state(&self.actor_type_name, &self.actor_identity)
            .await?;

        match stored {
            Some(entry) => {
                self.value = serde_json::from_slice(&entry.data)
                    .map_err(|e| ActorStateError::SerializationError(e.to_string()))?;
                self.etag = Some(entry.etag);
                self.record_exists = true;
            }
            None => {
                self.value = T::default();
                self.etag = None;
                self.record_exists = false;
            }
        }
        Ok(())
    }

    /// Clear the state from the store and reset to default.
    pub async fn clear_state(&mut self) -> Result<(), ActorStateError> {
        self.store
            .clear_state(
                &self.actor_type_name,
                &self.actor_identity,
                self.etag.as_deref(),
            )
            .await?;

        self.value = T::default();
        self.etag = None;
        self.record_exists = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::InMemoryStateStore;
    use serde::Deserialize;

    #[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestData {
        count: i64,
        name: String,
    }

    #[tokio::test]
    async fn test_load_new_state() {
        let store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

        let ps = PersistentState::<TestData>::load(store, "TestActor", "alice")
            .await
            .expect("load should succeed");

        assert!(!ps.record_exists());
        assert!(ps.etag().is_none());
        assert_eq!(ps.state().count, 0);
        assert_eq!(ps.state().name, "");
    }

    #[tokio::test]
    async fn test_write_and_reload() {
        let store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

        // Write
        {
            let mut ps = PersistentState::<TestData>::load(store.clone(), "TestActor", "alice")
                .await
                .expect("load");
            ps.state_mut().count = 42;
            ps.state_mut().name = "Alice".to_string();
            ps.write_state().await.expect("write");

            assert!(ps.record_exists());
            assert!(ps.etag().is_some());
        }

        // Reload
        {
            let ps = PersistentState::<TestData>::load(store, "TestActor", "alice")
                .await
                .expect("reload");

            assert!(ps.record_exists());
            assert_eq!(ps.state().count, 42);
            assert_eq!(ps.state().name, "Alice");
        }
    }

    #[tokio::test]
    async fn test_etag_update_on_write() {
        let store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

        let mut ps = PersistentState::<TestData>::load(store, "TestActor", "alice")
            .await
            .expect("load");

        ps.state_mut().count = 1;
        ps.write_state().await.expect("write 1");
        let etag1 = ps.etag().expect("etag after write 1").to_string();

        ps.state_mut().count = 2;
        ps.write_state().await.expect("write 2");
        let etag2 = ps.etag().expect("etag after write 2").to_string();

        assert_ne!(etag1, etag2);
    }

    #[tokio::test]
    async fn test_clear_state() {
        let store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

        let mut ps = PersistentState::<TestData>::load(store.clone(), "TestActor", "alice")
            .await
            .expect("load");
        ps.state_mut().count = 100;
        ps.write_state().await.expect("write");

        ps.clear_state().await.expect("clear");
        assert!(!ps.record_exists());
        assert!(ps.etag().is_none());
        assert_eq!(ps.state().count, 0); // reset to default

        // Store should be empty
        let reloaded = PersistentState::<TestData>::load(store, "TestActor", "alice")
            .await
            .expect("reload");
        assert!(!reloaded.record_exists());
    }

    #[tokio::test]
    async fn test_read_state_refreshes() {
        let store: Rc<dyn ActorStateStore> = Rc::new(InMemoryStateStore::new());

        let mut ps = PersistentState::<TestData>::load(store.clone(), "TestActor", "alice")
            .await
            .expect("load");
        ps.state_mut().count = 10;
        ps.write_state().await.expect("write");

        // Write directly to store (simulating another actor's write — unlikely but tests read_state)
        let data = serde_json::to_vec(&TestData {
            count: 99,
            name: "external".to_string(),
        })
        .expect("serialize");
        let _new_etag = store
            .write_state("TestActor", "alice", data, ps.etag())
            .await
            .expect("external write");

        // Now read_state should pick up the external change
        ps.read_state().await.expect("read_state");
        assert_eq!(ps.state().count, 99);
        assert_eq!(ps.state().name, "external");
    }
}
