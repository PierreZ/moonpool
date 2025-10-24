//! Actor state wrapper for typed persistence.

use crate::actor::ActorId;
use crate::error::ActorError;
use crate::storage::{JsonSerializer, StateSerializer, StorageProvider};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;

/// Type-safe wrapper for actor state with automatic persistence.
///
/// ActorState<T> provides a clean API for managing actor state with
/// automatic serialization and storage integration.
///
/// # Type Parameters
///
/// - `T`: The state type (must implement Serialize + Deserialize + Clone)
///
/// # Example
///
/// ```rust,no_run
/// use moonpool::actor::ActorState;
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Debug, Clone, Serialize, Deserialize)]
/// struct BankAccountState {
///     balance: u64,
/// }
///
/// struct BankAccountActor {
///     state: ActorState<BankAccountState>,
/// }
///
/// impl BankAccountActor {
///     async fn deposit(&mut self, amount: u64) -> Result<(), ActorError> {
///         let mut state = self.state.get().clone();
///         state.balance += amount;
///         self.state.set(state);
///         self.state.persist().await?;
///         Ok(())
///     }
/// }
/// ```
pub struct ActorState<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
{
    actor_id: ActorId,
    state: RefCell<T>,
    storage: Rc<dyn StorageProvider>,
    serializer: JsonSerializer,
    dirty: RefCell<bool>,
}

impl<T> std::fmt::Debug for ActorState<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorState")
            .field("actor_id", &self.actor_id)
            .field("state", &self.state)
            .field("dirty", &self.dirty)
            .finish()
    }
}

impl<T> ActorState<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
{
    /// Create a new ActorState with the given initial state.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The actor's unique identifier
    /// - `initial_state`: The initial state value
    /// - `storage`: Storage provider for persistence
    pub fn new(actor_id: ActorId, initial_state: T, storage: Rc<dyn StorageProvider>) -> Self {
        Self {
            actor_id,
            state: RefCell::new(initial_state),
            storage,
            serializer: JsonSerializer::new(),
            dirty: RefCell::new(false),
        }
    }

    /// Get a reference to the current state.
    ///
    /// # Returns
    ///
    /// A reference to the current state value.
    pub fn get(&self) -> std::cell::Ref<'_, T> {
        self.state.borrow()
    }

    /// Get a mutable reference to the current state.
    ///
    /// Marks the state as dirty, requiring persistence.
    ///
    /// # Returns
    ///
    /// A mutable reference to the current state value.
    pub fn get_mut(&self) -> std::cell::RefMut<'_, T> {
        *self.dirty.borrow_mut() = true;
        self.state.borrow_mut()
    }

    /// Set the state to a new value.
    ///
    /// Marks the state as dirty, requiring persistence.
    ///
    /// # Parameters
    ///
    /// - `new_state`: The new state value
    pub fn set(&self, new_state: T) {
        *self.state.borrow_mut() = new_state;
        *self.dirty.borrow_mut() = true;
    }

    /// Check if the state has been modified since the last persist.
    ///
    /// # Returns
    ///
    /// `true` if the state is dirty and needs to be persisted.
    pub fn is_dirty(&self) -> bool {
        *self.dirty.borrow()
    }

    /// Persist the current state to storage.
    ///
    /// This method serializes the state and saves it to the storage backend.
    /// Only persists if the state is dirty.
    ///
    /// # Returns
    ///
    /// - `Ok(())`: State successfully persisted
    /// - `Err(ActorError)`: Failed to persist state
    #[allow(clippy::await_holding_refcell_ref)]
    pub async fn persist(&self) -> Result<(), ActorError> {
        if !self.is_dirty() {
            return Ok(()); // No changes to persist
        }

        // Note: We serialize while holding the borrow, then await. This is safe
        // in single-threaded context as no other code can access the RefCell.
        let state = self.state.borrow();
        let bytes = self.serializer.serialize(&*state).map_err(|e| {
            ActorError::ProcessingFailed(format!("State serialization failed: {}", e))
        })?;

        self.storage
            .save_state(&self.actor_id, bytes)
            .await
            .map_err(|e| ActorError::ProcessingFailed(format!("State save failed: {}", e)))?;

        *self.dirty.borrow_mut() = false;
        Ok(())
    }

    /// Load state from storage.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The actor's unique identifier
    /// - `default_state`: Default state if none exists in storage
    /// - `storage`: Storage provider for persistence
    ///
    /// # Returns
    ///
    /// - `Ok(ActorState)`: State loaded or created with default
    /// - `Err(ActorError)`: Failed to load state
    pub async fn load(
        actor_id: ActorId,
        default_state: T,
        storage: Rc<dyn StorageProvider>,
    ) -> Result<Self, ActorError> {
        let serializer = JsonSerializer::new();

        match storage.load_state(&actor_id).await {
            Ok(Some(bytes)) => {
                let state: T = serializer.deserialize(&bytes).map_err(|e| {
                    ActorError::ProcessingFailed(format!("State deserialization failed: {}", e))
                })?;

                Ok(Self {
                    actor_id,
                    state: RefCell::new(state),
                    storage,
                    serializer,
                    dirty: RefCell::new(false),
                })
            }
            Ok(None) => {
                // No state in storage, use default
                Ok(Self::new(actor_id, default_state, storage))
            }
            Err(e) => Err(ActorError::ProcessingFailed(format!(
                "State load failed: {}",
                e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{InMemoryStorage, JsonSerializer};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestState {
        counter: i32,
        name: String,
    }

    #[tokio::test]
    async fn test_actor_state_get_set() {
        let actor_id = ActorId::new("test", "TestActor", "test-actor");
        let storage = Rc::new(InMemoryStorage::new());

        let initial = TestState {
            counter: 0,
            name: "test".to_string(),
        };
        let state = ActorState::new(actor_id, initial, storage);

        // Get initial state
        assert_eq!(state.get().counter, 0);
        assert!(!state.is_dirty());

        // Modify via set
        state.set(TestState {
            counter: 42,
            name: "updated".to_string(),
        });

        assert_eq!(state.get().counter, 42);
        assert!(state.is_dirty());
    }

    #[tokio::test]
    async fn test_actor_state_get_mut() {
        let actor_id = ActorId::new("test", "TestActor", "test-actor");
        let storage = Rc::new(InMemoryStorage::new());

        let initial = TestState {
            counter: 0,
            name: "test".to_string(),
        };
        let state = ActorState::new(actor_id, initial, storage);

        // Modify via get_mut
        {
            let mut s = state.get_mut();
            s.counter = 100;
        }

        assert_eq!(state.get().counter, 100);
        assert!(state.is_dirty());
    }

    #[tokio::test]
    async fn test_actor_state_persist() {
        let actor_id = ActorId::new("test", "TestActor", "test-actor");
        let storage = Rc::new(InMemoryStorage::new());

        let initial = TestState {
            counter: 0,
            name: "test".to_string(),
        };
        let state = ActorState::new(actor_id.clone(), initial, storage.clone());

        // Modify and persist
        state.set(TestState {
            counter: 42,
            name: "persisted".to_string(),
        });

        assert!(state.is_dirty());
        state.persist().await.unwrap();
        assert!(!state.is_dirty());

        // Verify storage has the data
        let loaded = storage.load_state(&actor_id).await.unwrap();
        assert!(loaded.is_some());
    }

    #[tokio::test]
    async fn test_actor_state_load_existing() {
        let actor_id = ActorId::new("test", "TestActor", "test-actor");
        let storage = Rc::new(InMemoryStorage::new());
        let serializer = JsonSerializer::new();

        // Save some state
        let initial = TestState {
            counter: 99,
            name: "saved".to_string(),
        };
        let bytes = serializer.serialize(&initial).unwrap();
        storage.save_state(&actor_id, bytes).await.unwrap();

        // Load it back
        let default = TestState {
            counter: 0,
            name: "default".to_string(),
        };
        let state = ActorState::load(actor_id, default, storage).await.unwrap();

        assert_eq!(state.get().counter, 99);
        assert_eq!(state.get().name, "saved");
        assert!(!state.is_dirty());
    }

    #[tokio::test]
    async fn test_actor_state_load_default() {
        let actor_id = ActorId::new("test", "TestActor", "new-actor");
        let storage = Rc::new(InMemoryStorage::new());

        let default = TestState {
            counter: 100,
            name: "default".to_string(),
        };
        let state = ActorState::load(actor_id, default.clone(), storage)
            .await
            .unwrap();

        assert_eq!(state.get().counter, 100);
        assert_eq!(state.get().name, "default");
    }

    #[tokio::test]
    async fn test_actor_state_persist_when_not_dirty() {
        let actor_id = ActorId::new("test", "TestActor", "test-actor");
        let storage = Rc::new(InMemoryStorage::new());

        let initial = TestState {
            counter: 0,
            name: "test".to_string(),
        };
        let state = ActorState::new(actor_id.clone(), initial, storage.clone());

        // Persist without modifications
        state.persist().await.unwrap();

        // Storage should still be empty (no unnecessary writes)
        assert!(storage.is_empty());
    }
}
