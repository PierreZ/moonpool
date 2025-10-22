//! In-memory storage implementation for testing.

use crate::actor::ActorId;
use crate::storage::error::StorageError;
use crate::storage::traits::StorageProvider;
use async_trait::async_trait;
use std::cell::RefCell;
use std::collections::HashMap;

/// Simple in-memory storage implementation using HashMap.
///
/// This implementation is suitable for testing and single-node development.
/// State is stored in memory and will be lost when the process terminates.
#[derive(Debug)]
pub struct InMemoryStorage {
    data: RefCell<HashMap<String, Vec<u8>>>,
}

impl InMemoryStorage {
    /// Create a new empty in-memory storage.
    pub fn new() -> Self {
        Self {
            data: RefCell::new(HashMap::new()),
        }
    }

    /// Get the number of stored actor states.
    pub fn len(&self) -> usize {
        self.data.borrow().len()
    }

    /// Check if the storage is empty.
    pub fn is_empty(&self) -> bool {
        self.data.borrow().is_empty()
    }

    /// Clear all stored state.
    pub fn clear(&self) {
        self.data.borrow_mut().clear();
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl StorageProvider for InMemoryStorage {
    async fn load_state(&self, actor_id: &ActorId) -> Result<Option<Vec<u8>>, StorageError> {
        let key = actor_id.to_string();
        Ok(self.data.borrow().get(&key).cloned())
    }

    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError> {
        let key = actor_id.to_string();
        self.data.borrow_mut().insert(key, data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_memory_storage_save_and_load() {
        let storage = InMemoryStorage::new();
        let actor_id = ActorId::new("test", "TestActor", "test-actor");
        let data = vec![1, 2, 3, 4];

        // Save state
        storage.save_state(&actor_id, data.clone()).await.unwrap();

        // Load state
        let loaded = storage.load_state(&actor_id).await.unwrap();
        assert_eq!(loaded, Some(data));
    }

    #[tokio::test]
    async fn test_in_memory_storage_load_nonexistent() {
        let storage = InMemoryStorage::new();
        let actor_id = ActorId::new("test", "TestActor", "nonexistent");

        let loaded = storage.load_state(&actor_id).await.unwrap();
        assert_eq!(loaded, None);
    }

    #[tokio::test]
    async fn test_in_memory_storage_overwrite() {
        let storage = InMemoryStorage::new();
        let actor_id = ActorId::new("test", "TestActor", "test-actor");

        // Save initial state
        storage.save_state(&actor_id, vec![1, 2, 3]).await.unwrap();

        // Overwrite with new state
        let new_data = vec![4, 5, 6];
        storage
            .save_state(&actor_id, new_data.clone())
            .await
            .unwrap();

        // Load should return new state
        let loaded = storage.load_state(&actor_id).await.unwrap();
        assert_eq!(loaded, Some(new_data));
    }

    #[tokio::test]
    async fn test_in_memory_storage_clear() {
        let storage = InMemoryStorage::new();
        let actor_id = ActorId::new("test", "TestActor", "test-actor");

        storage.save_state(&actor_id, vec![1, 2, 3]).await.unwrap();
        assert_eq!(storage.len(), 1);

        storage.clear();
        assert_eq!(storage.len(), 0);

        let loaded = storage.load_state(&actor_id).await.unwrap();
        assert_eq!(loaded, None);
    }
}
