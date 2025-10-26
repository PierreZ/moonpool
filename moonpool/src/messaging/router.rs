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
    /// - `message_bus`: MessageBus reference for spawning message loops and sending responses
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
    /// 2. Get or create actor activation (passing message_bus for message loop)
    /// 3. Enqueue message in actor's queue
    /// 4. Message processing task spawned automatically if needed
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// async fn route_message(&self, message: Message, message_bus: Rc<MessageBus>) -> Result<(), ActorError> {
    ///     // 1. Get or create activation (passing message_bus)
    ///     let context = self.get_or_create_activation(
    ///         message.target_actor.clone(),
    ///         message_bus
    ///     ).await?;
    ///
    ///     // 2. Enqueue message (message loop processes it)
    ///     context.enqueue_message(message).await?;
    ///
    ///     Ok(())
    /// }
    /// ```
    async fn route_message(
        &self,
        message: Message,
        message_bus: std::rc::Rc<crate::messaging::MessageBus>,
    ) -> Result<(), ActorError>;

    /// Get the placement hint for this actor type.
    ///
    /// This method returns the placement preference for actors of this type,
    /// which influences where new actors should be activated in the cluster.
    ///
    /// # Returns
    ///
    /// The PlacementHint for this actor type (Local, Random, or LeastLoaded).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let hint = router.placement_hint();
    /// let target_node = directory.choose_placement_node(hint, my_node).await?;
    /// ```
    fn placement_hint(&self) -> crate::actor::PlacementHint;
}
