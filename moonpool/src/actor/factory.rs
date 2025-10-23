//! Actor factory for dynamic actor instantiation.
//!
//! This module provides the `ActorFactory` trait which enables the catalog to
//! create actor instances on-demand without requiring user intervention.
//!
//! # Orleans Pattern
//!
//! Orleans uses `GrainActivator.CreateInstance(address)` to create grain instances
//! via dependency injection. This pattern enables:
//! - Auto-activation on first message
//! - Dependency injection for actors
//! - Separation of instantiation logic from lifecycle management
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool::actor::{Actor, ActorFactory, ActorId};
//!
//! struct BankAccountFactory;
//!
//! #[async_trait(?Send)]
//! impl ActorFactory for BankAccountFactory {
//!     type Actor = BankAccountActor;
//!
//!     async fn create(&self, actor_id: ActorId) -> Result<Self::Actor, ActorError> {
//!         Ok(BankAccountActor::new(actor_id))
//!     }
//! }
//!
//! // Register with runtime
//! runtime.register_actor::<BankAccountActor, _>(BankAccountFactory)?;
//!
//! // Actors now auto-activate on first message!
//! let alice = runtime.get_actor::<BankAccountActor>("BankAccount", "alice")?;
//! alice.call(DepositRequest { amount: 100 }).await?;
//! ```

use crate::actor::{Actor, ActorId};
use crate::error::ActorError;
use async_trait::async_trait;

/// Factory for creating actor instances on-demand.
///
/// Separates actor instantiation logic from catalog management, enabling:
/// - **Auto-activation**: Actors activate automatically on first message
/// - **Dependency injection**: Factory can inject services, config, etc.
/// - **Testability**: Easy to mock factories for testing
///
/// # Pattern
///
/// This follows Orleans' `IGrainActivator` pattern where the catalog delegates
/// actor creation to a factory instead of requiring pre-constructed instances.
///
/// # Implementation
///
/// ```rust,ignore
/// use moonpool::actor::{ActorFactory, Actor, ActorId};
///
/// struct BankAccountFactory {
///     db_pool: Arc<DatabasePool>,  // Dependency injection
/// }
///
/// #[async_trait(?Send)]
/// impl ActorFactory for BankAccountFactory {
///     type Actor = BankAccountActor;
///
///     async fn create(&self, actor_id: ActorId) -> Result<Self::Actor, ActorError> {
///         // Could load config, check permissions, etc.
///         Ok(BankAccountActor::new(actor_id, self.db_pool.clone()))
///     }
/// }
/// ```
#[async_trait(?Send)]
pub trait ActorFactory {
    /// The actor type this factory creates.
    type Actor: Actor;

    /// Create a new actor instance for the given ActorId.
    ///
    /// This method is called by `ActorCatalog::get_or_create_activation()` when
    /// activating an actor for the first time. It's invoked **inside the
    /// activation lock**, so the actor instance is never discarded.
    ///
    /// # Parameters
    ///
    /// - `actor_id`: The unique identifier for the actor to create
    ///
    /// # Returns
    ///
    /// - `Ok(actor)`: The newly created actor instance
    /// - `Err(ActorError)`: If actor creation fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn create(&self, actor_id: ActorId) -> Result<BankAccountActor, ActorError> {
    ///     // Validate actor ID
    ///     if actor_id.key.is_empty() {
    ///         return Err(ActorError::InvalidConfiguration("Actor key required".into()));
    ///     }
    ///
    ///     // Create actor with dependencies
    ///     Ok(BankAccountActor::new(actor_id, self.services.clone()))
    /// }
    /// ```
    async fn create(&self, actor_id: ActorId) -> Result<Self::Actor, ActorError>;
}
