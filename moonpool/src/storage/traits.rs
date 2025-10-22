//! Storage provider trait abstraction.

use crate::actor::ActorId;
use crate::storage::error::StorageError;
use async_trait::async_trait;

/// Trait for loading and saving actor state.
///
/// Implementations provide a backend for persisting actor state as raw bytes.
/// The actual serialization/deserialization is handled by StateSerializer.
#[async_trait(?Send)]
pub trait StorageProvider {
    /// Load the state for a given actor.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The unique identifier of the actor
    ///
    /// # Returns
    ///
    /// - `Ok(Some(bytes))`: State data found and loaded
    /// - `Ok(None)`: No state exists for this actor (new activation)
    /// - `Err(StorageError)`: Failed to load state
    async fn load_state(&self, actor_id: &ActorId) -> Result<Option<Vec<u8>>, StorageError>;

    /// Save the state for a given actor.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The unique identifier of the actor
    /// - `data`: Serialized state bytes to persist
    ///
    /// # Returns
    ///
    /// - `Ok(())`: State successfully saved
    /// - `Err(StorageError)`: Failed to save state
    async fn save_state(&self, actor_id: &ActorId, data: Vec<u8>) -> Result<(), StorageError>;
}
