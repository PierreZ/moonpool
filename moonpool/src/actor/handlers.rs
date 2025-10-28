//! Dynamic message dispatch via handler registry.
//!
//! This module provides the `HandlerRegistry` which maps method names to
//! type-erased handler closures, enabling dynamic dispatch from string-based
//! message names to strongly-typed `MessageHandler` trait implementations.

use crate::actor::{Actor, ActorContext, MessageHandler};
use crate::error::ActorError;
use crate::messaging::Message;
use crate::serialization::Serializer;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

/// Type-erased handler function.
///
/// This is a boxed closure that:
/// 1. Takes a mutable actor reference, message, and context
/// 2. Returns a pinned future that produces a serialized response
///
/// The closure internally:
/// - Deserializes `Message.payload` to the request type
/// - Calls `MessageHandler::handle()`
/// - Serializes the response to `Vec<u8>`
///
/// Uses higher-ranked trait bound (HRTB) `for<'a>` to handle lifetimes correctly.
type HandlerFn<A, S> = Box<
    dyn for<'a> Fn(
        &'a mut A,
        &'a Message,
        &'a ActorContext<A, S>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, ActorError>> + 'a>>,
>;

/// Registry mapping method names to type-erased handler functions.
///
/// The `HandlerRegistry` solves the dynamic dispatch problem in Rust actor systems:
/// - Messages arrive with string-based `method_name` (e.g., "SayHelloRequest")
/// - Need to call the correct `MessageHandler<Req, Res>::handle()` implementation
/// - Rust doesn't have runtime reflection to map strings → trait methods
///
/// Solution: Actors register handlers during initialization, creating type-erased
/// closures that perform deserialization → handler call → serialization.
///
/// # Architecture
///
/// ```text
/// Message { method_name: "SayHelloRequest", payload: [...] }
///   ↓
/// registry.dispatch(actor, message, ctx)
///   ↓
/// HandlerFn closure:
///   1. Deserialize payload → SayHelloRequest
///   2. Call actor.handle(req, ctx) → Result<String, ActorError>
///   3. Serialize String → Vec<u8>
///   ↓
/// Response payload sent back to caller
/// ```
///
/// # Usage
///
/// Actors register handlers in their `Actor::register_handlers()` implementation:
///
/// ```rust,ignore
/// impl Actor for HelloActor {
///     fn register_handlers(registry: &mut HandlerRegistry<Self>) {
///         registry.register::<SayHelloRequest, String>();
///         registry.register::<GetStatusRequest, Status>();
///     }
/// }
/// ```
///
/// The framework calls `dispatch()` when messages arrive:
///
/// ```rust,ignore
/// let response_payload = context.handlers.dispatch(actor, message, context).await?;
/// ```
pub struct HandlerRegistry<A: Actor, S: Serializer> {
    /// Map from method name to handler function.
    ///
    /// Key: Type name of request (e.g., "SayHelloRequest")
    /// Value: Type-erased closure that handles the message
    handlers: HashMap<String, HandlerFn<A, S>>,

    /// Message serializer for encoding/decoding message payloads.
    ///
    /// Pluggable serialization. Used to deserialize incoming request payloads
    /// and serialize outgoing response payloads. Generic over Serializer trait
    /// for maximum flexibility (users can provide their own implementations).
    message_serializer: S,
}

impl<A: Actor, S: Serializer + Clone + 'static> HandlerRegistry<A, S> {
    /// Create a new empty handler registry with the given message serializer.
    ///
    /// # Parameters
    ///
    /// - `message_serializer`: Serializer for message payloads
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use moonpool::serialization::JsonSerializer;
    ///
    /// let serializer = JsonSerializer;
    /// let registry = HandlerRegistry::new(serializer);
    /// ```
    pub fn new(message_serializer: S) -> Self {
        Self {
            handlers: HashMap::new(),
            message_serializer,
        }
    }

    /// Register a handler for a specific request/response message pair.
    ///
    /// This method creates a type-erased closure that:
    /// 1. Deserializes `Message.payload` to `Req` using serde_json
    /// 2. Calls `actor.handle(req, ctx)` via the `MessageHandler<Req, Res>` trait
    /// 3. Serializes the `Res` response to `Vec<u8>` using serde_json
    ///
    /// # Type Parameters
    ///
    /// - `Req`: Request message type (must implement `DeserializeOwned`)
    /// - `Res`: Response type (must implement `Serialize`)
    ///
    /// # Constraints
    ///
    /// - `A` must implement `MessageHandler<Req, Res>`
    /// - Request type name (via `std::any::type_name`) is used as the method name
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut registry = HandlerRegistry::<HelloActor>::new();
    /// registry.register::<SayHelloRequest, String>();
    /// ```
    ///
    /// # Method Name Extraction
    ///
    /// The method name is extracted from the fully-qualified type name:
    /// - `my_crate::messages::SayHelloRequest` → `"SayHelloRequest"`
    /// - Must match the `method_name` field in incoming `Message` structs
    pub fn register<Req, Res>(&mut self)
    where
        A: MessageHandler<Req, Res, S>,
        Req: serde::Serialize + serde::de::DeserializeOwned,
        Res: serde::Serialize + serde::de::DeserializeOwned,
    {
        // Extract simple type name from fully-qualified path
        let method_name = std::any::type_name::<Req>()
            .rsplit("::")
            .next()
            .unwrap_or("unknown")
            .to_string();

        tracing::debug!(
            "Registering handler for method '{}' on actor type '{}'",
            method_name,
            std::any::type_name::<A>()
        );

        // Clone method_name and serializer for use in closure
        let method_name_for_closure = method_name.clone();
        let serializer = self.message_serializer.clone();

        // Create type-erased handler closure
        let handler: HandlerFn<A, S> = Box::new(
            move |actor: &mut A, message: &Message, ctx: &ActorContext<A, S>| {
                // Clone method_name and serializer for the async block
                let method_name = method_name_for_closure.clone();
                let serializer = serializer.clone();

                Box::pin(async move {
                    // 1. Deserialize request payload using configured serializer
                    let req: Req = serializer.deserialize(&message.payload).map_err(|e| {
                        ActorError::Message(crate::error::MessageError::Serialization(format!(
                            "Failed to deserialize {}: {}",
                            method_name, e
                        )))
                    })?;

                    // 2. Call the MessageHandler::handle() method
                    // This is where the user's handler logic executes
                    let res: Res = actor.handle(req, ctx).await?;

                    // 3. Serialize response using configured serializer
                    let payload = serializer.serialize(&res).map_err(|e| {
                        ActorError::Message(crate::error::MessageError::Serialization(format!(
                            "Failed to serialize response for {}: {}",
                            method_name, e
                        )))
                    })?;

                    Ok(payload)
                })
            },
        );

        // Insert into registry
        self.handlers.insert(method_name, handler);
    }

    /// Dispatch a message to the appropriate handler.
    ///
    /// This is the core dispatch mechanism called by the message loop.
    ///
    /// # Parameters
    ///
    /// - `actor`: Mutable reference to the actor instance
    /// - `message`: Incoming message with method name and serialized payload
    /// - `ctx`: Actor execution context
    ///
    /// # Returns
    ///
    /// - `Ok(Vec<u8>)`: Serialized response payload
    /// - `Err(ActorError::ProcessingFailed)`: No handler registered for method
    /// - `Err(ActorError::Message)`: Serialization/deserialization failed
    /// - `Err(ActorError::*)`: Handler execution failed
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // In message loop:
    /// let response_payload = context.handlers.dispatch(actor, message, context).await?;
    /// let response = Message::response(message, response_payload);
    /// message_bus.send_response(response).await?;
    /// ```
    pub async fn dispatch(
        &self,
        actor: &mut A,
        message: &Message,
        ctx: &ActorContext<A, S>,
    ) -> Result<Vec<u8>, ActorError> {
        // Lookup handler by method name
        let handler = self.handlers.get(&message.method_name).ok_or_else(|| {
            ActorError::ProcessingFailed(format!(
                "No handler registered for method '{}' on actor {}",
                message.method_name, ctx.actor_id
            ))
        })?;

        // Execute handler closure
        handler(actor, message, ctx).await
    }

    /// Check if a handler exists for a method.
    ///
    /// Useful for validation or debugging.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if registry.has_handler("SayHelloRequest") {
    ///     println!("Handler registered");
    /// }
    /// ```
    pub fn has_handler(&self, method_name: &str) -> bool {
        self.handlers.contains_key(method_name)
    }

    /// Get the number of registered handlers.
    ///
    /// Useful for debugging and testing.
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }
}

// No Default impl - users must explicitly provide serializer
// This enforces the design goal of making serialization pluggable without defaults

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorId, DeactivationReason};
    use crate::serialization::JsonSerializer;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};

    // Test actor
    struct TestActor {
        actor_id: ActorId,
        value: String,
    }

    #[async_trait(?Send)]
    impl Actor for TestActor {
        type State = ();
        const ACTOR_TYPE: &'static str = "TestActor";

        fn actor_id(&self) -> &ActorId {
            &self.actor_id
        }

        async fn on_activate(
            &mut self,
            _state: crate::actor::ActorState<Self::State>,
        ) -> Result<(), ActorError> {
            Ok(())
        }

        async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
            Ok(())
        }
    }

    // Test message types
    #[derive(Debug, Serialize, Deserialize)]
    struct SetValueRequest {
        value: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct GetValueRequest;

    // Handler implementations
    #[async_trait(?Send)]
    impl<S: Serializer> MessageHandler<SetValueRequest, (), S> for TestActor {
        async fn handle(
            &mut self,
            req: SetValueRequest,
            _ctx: &ActorContext<Self, S>,
        ) -> Result<(), ActorError> {
            self.value = req.value;
            Ok(())
        }
    }

    #[async_trait(?Send)]
    impl<S: Serializer> MessageHandler<GetValueRequest, String, S> for TestActor {
        async fn handle(
            &mut self,
            _req: GetValueRequest,
            _ctx: &ActorContext<Self, S>,
        ) -> Result<String, ActorError> {
            Ok(self.value.clone())
        }
    }

    #[test]
    fn test_handler_registry_creation() {
        let serializer = JsonSerializer;
        let registry = HandlerRegistry::<TestActor, _>::new(serializer);
        assert_eq!(registry.handler_count(), 0);
    }

    #[test]
    fn test_handler_registration() {
        let serializer = JsonSerializer;
        let mut registry = HandlerRegistry::<TestActor, _>::new(serializer);
        registry.register::<SetValueRequest, ()>();
        registry.register::<GetValueRequest, String>();

        assert_eq!(registry.handler_count(), 2);
        assert!(registry.has_handler("SetValueRequest"));
        assert!(registry.has_handler("GetValueRequest"));
        assert!(!registry.has_handler("NonExistentRequest"));
    }
}
