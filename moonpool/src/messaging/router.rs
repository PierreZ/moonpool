//! Actor router trait for type-erased message routing.
//!
//! This module defines the `ActorRouter` trait which enables MessageBus to
//! route messages to ActorCatalog without requiring generic parameters.

use crate::error::ActorError;
use crate::messaging::Message;
use async_trait::async_trait;

/// Trait for routing messages to local actors (type-erased).
///
/// This trait provides a type-erased interface for routing messages to actors,
/// allowing MessageBus to remain non-generic while still being able to deliver
/// messages to ActorCatalog<A> instances.
///
/// # Design Rationale
///
/// Without this trait, MessageBus would need to be generic over actor type `A`,
/// which creates several problems:
/// 1. MessageBus couldn't handle multiple actor types simultaneously
/// 2. ActorRuntime would become generic and complex
/// 3. User code would need to specify types everywhere
///
/// The ActorRouter trait solves this by providing dynamic dispatch while
/// maintaining type safety within each ActorCatalog implementation.
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::messaging::ActorRouter;
///
/// // ActorCatalog implements ActorRouter
/// impl<A: Actor> ActorRouter for ActorCatalog<A> {
///     async fn route_message(&self, message: Message) -> Result<(), ActorError> {
///         // Get or create activation
///         let context = self.get_or_create_activation(
///             message.target_actor.clone()
///         ).await?;
///
///         // Enqueue message
///         context.enqueue_message(message);
///
///         Ok(())
///     }
/// }
///
/// // MessageBus can route to any ActorRouter
/// let router: Rc<dyn ActorRouter> = Rc::new(catalog);
/// bus.set_actor_router(router);
/// ```
#[async_trait(?Send)]
pub trait ActorRouter {
    /// Route a message to the appropriate local actor.
    ///
    /// # Parameters
    ///
    /// - `message`: The message to route to an actor
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Message successfully routed to actor's queue
    /// - `Err(ActorError)`: Routing failed (actor not found, activation failed, etc.)
    ///
    /// # Implementation Notes
    ///
    /// Implementations should:
    /// 1. Extract target actor ID from message
    /// 2. Get or create actor activation
    /// 3. Enqueue message in actor's queue
    /// 4. Spawn message processing task if not already running
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn route_message(&self, message: Message) -> Result<(), ActorError> {
    ///     // 1. Get or create activation
    ///     let context = self.get_or_create_activation(
    ///         message.target_actor.clone()
    ///     )?;
    ///
    ///     // 2. Enqueue message
    ///     context.enqueue_message(message.clone());
    ///
    ///     // 3. Spawn processing task if needed
    ///     if !context.is_processing() {
    ///         context.set_processing(true);
    ///         self.spawn_message_processor(context).await;
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    async fn route_message(&self, message: Message) -> Result<(), ActorError>;
}
