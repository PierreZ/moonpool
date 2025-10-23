//! Actor reference for type-safe remote actor invocation.

use crate::actor::{Actor, ActorId};
use crate::error::ActorError;
use crate::messaging::{Message, MessageBus};
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;

/// Type-safe reference to an actor that may be local or remote.
///
/// `ActorRef<A>` provides a handle to invoke methods on an actor without
/// knowing its physical location. The actor system handles:
/// - Directory lookup to find the actor's current node
/// - Automatic activation if the actor is not yet running
/// - Message routing across the network
/// - Request-response correlation
///
/// # Type Safety
///
/// The generic parameter `A` enforces that only messages compatible with
/// actor type `A` can be sent through this reference.
///
/// # Example
///
/// ```rust,ignore
/// use moonpool::prelude::*;
///
/// // Get actor reference (no network call, returns immediately)
/// let alice: ActorRef<BankAccountActor> =
///     runtime.get_actor("BankAccount", "alice")?;
///
/// // Send request and await response
/// let balance = alice.call(DepositRequest { amount: 100 }).await?;
///
/// // With custom timeout
/// let balance = alice.call_with_timeout(
///     GetBalanceRequest,
///     Duration::from_secs(5)
/// ).await?;
///
/// // Fire-and-forget (no response)
/// alice.send(LogEventRequest { event: "deposit".into() }).await?;
/// ```
///
/// # Lifecycle
///
/// - Created via `ActorRuntime::get_actor()`
/// - First message triggers automatic activation
/// - Reference remains valid even if actor deactivates
/// - Can be cloned cheaply (just copies ActorId)
pub struct ActorRef<A: Actor> {
    /// Unique identifier for the target actor.
    actor_id: ActorId,

    /// Message bus for sending messages.
    ///
    /// Optional for backwards compatibility (tests can use ActorRef::new without bus).
    /// In production, this is always Some() when created via ActorRuntime::get_actor().
    message_bus: Option<Rc<MessageBus>>,

    /// Phantom marker to enforce type safety.
    _phantom: PhantomData<A>,
}

impl<A: Actor> ActorRef<A> {
    /// Create a new ActorRef without MessageBus (for testing).
    ///
    /// Normally you get ActorRef from `ActorRuntime::get_actor()`, but this
    /// constructor is public for testing and advanced use cases.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let actor_id = ActorId::from_parts("prod", "BankAccount", "alice")?;
    /// let actor_ref = ActorRef::<BankAccountActor>::new(actor_id);
    /// ```
    pub fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            message_bus: None,
            _phantom: PhantomData,
        }
    }

    /// Create a new ActorRef with MessageBus.
    ///
    /// This is the recommended way to create ActorRef instances outside of ActorRuntime.
    /// For tests and manual actor system setup.
    pub fn with_message_bus(actor_id: ActorId, message_bus: Rc<MessageBus>) -> Self {
        Self {
            actor_id,
            message_bus: Some(message_bus),
            _phantom: PhantomData,
        }
    }

    /// Get the actor ID.
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// Send a request and await the response.
    ///
    /// This is the primary method for request-response communication.
    /// The call will:
    /// 1. Look up the actor's location in the directory
    /// 2. Activate the actor if not already running
    /// 3. Send the request with correlation ID
    /// 4. Await the response with default timeout (30 seconds)
    /// 5. Deserialize and return the response
    ///
    /// # Type Parameters
    ///
    /// - `Req`: Request message type (must be serializable)
    /// - `Resp`: Response type (must be deserializable)
    ///
    /// # Errors
    ///
    /// - `ActorError::Timeout` - Response not received within timeout
    /// - `ActorError::NodeUnavailable` - Target node is unreachable
    /// - `ActorError::ExecutionFailed` - Actor threw an exception
    /// - `ActorError::Message` - Serialization/deserialization error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let balance = alice.call(DepositRequest { amount: 100 }).await?;
    /// println!("New balance: {}", balance);
    /// ```
    pub async fn call<Req, Resp>(&self, request: Req) -> Result<Resp, ActorError>
    where
        Req: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        // Delegate to call_with_timeout with default 30-second timeout
        self.call_with_timeout(request, Duration::from_secs(30))
            .await
    }

    /// Send a request with custom timeout.
    ///
    /// Same as `call()` but allows specifying a custom timeout duration.
    ///
    /// # Parameters
    ///
    /// - `request`: The request message to send
    /// - `timeout`: Maximum time to wait for response
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Short timeout for quick operations
    /// let balance = alice.call_with_timeout(
    ///     GetBalanceRequest,
    ///     Duration::from_millis(100)
    /// ).await?;
    /// ```
    pub async fn call_with_timeout<Req, Resp>(
        &self,
        request: Req,
        timeout: Duration,
    ) -> Result<Resp, ActorError>
    where
        Req: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        // 1. Get message bus reference
        let message_bus = self.message_bus.as_ref().ok_or_else(|| {
            ActorError::ProcessingFailed(
                "ActorRef created without MessageBus (use ActorRuntime::get_actor())".to_string(),
            )
        })?;

        // 2. Serialize request
        let payload = serde_json::to_vec(&request).map_err(|e| {
            ActorError::Message(crate::error::MessageError::Serialization(e.to_string()))
        })?;

        // 3. Extract method name from type
        let method_name = std::any::type_name::<Req>()
            .rsplit("::")
            .next()
            .unwrap_or("unknown")
            .to_string();

        // 4. Create sender actor ID (use system actor for now)
        // TODO: In Phase 4, this will be the current actor's ID from ActorContext
        let sender_actor = ActorId::from_string("system::System/root")
            .map_err(|e| ActorError::ProcessingFailed(format!("Invalid sender ID: {}", e)))?;

        // 5. Get sender's node ID from MessageBus
        // Both target_node and sender_node initially set to sender (MessageBus routing will
        // determine actual target location via directory lookup)
        let sender_node = message_bus.node_id().clone();
        let target_node = sender_node.clone(); // Will be updated by routing logic

        // Create request message with all required parameters
        let message = Message::request(
            crate::actor::CorrelationId::new(0), // MessageBus will assign real correlation ID
            self.actor_id.clone(),               // target_actor
            sender_actor,                        // sender_actor
            target_node,                         // target_node
            sender_node,                         // sender_node
            method_name,                         // method_name
            payload,                             // payload
            timeout,                             // timeout
        );

        // 6. Send request and get response receiver (returns mutated message with real correlation ID)
        let (message, mut rx) = message_bus.send_request(message).await?;
        let correlation_id = message.correlation_id; // Save for logging

        // 7. Route the message to the actor (use the mutated message with correct correlation ID!)
        message_bus.route_message(message).await?;

        // 8. Poll for response with busy loop and timeout (workaround for LocalSet/oneshot waker issue)
        // The oneshot waker doesn't properly trigger in LocalSet, so we manually poll
        tracing::info!(
            "ActorRef: Starting busy loop for corr_id={}, timeout={:?}",
            correlation_id,
            timeout
        );
        let start = std::time::Instant::now();
        let response_message = loop {
            tokio::task::yield_now().await;

            // Check timeout
            if start.elapsed() >= timeout {
                tracing::warn!(
                    "ActorRef: Timeout waiting for response, corr_id={}",
                    correlation_id
                );
                return Err(ActorError::Timeout);
            }

            match rx.try_recv() {
                Ok(result) => {
                    tracing::info!(
                        "ActorRef: Received response in busy loop, corr_id={}",
                        correlation_id
                    );
                    // result is Result<Message, ActorError>
                    break result?;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Not ready yet, continue polling
                    tracing::debug!("ActorRef: try_recv() returned Empty, continuing...");
                    continue;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    return Err(ActorError::ProcessingFailed(
                        "Response channel closed unexpectedly".to_string(),
                    ));
                }
            }
        };

        // 10. Deserialize response
        let response: Resp = serde_json::from_slice(&response_message.payload).map_err(|e| {
            ActorError::Message(crate::error::MessageError::Serialization(e.to_string()))
        })?;

        Ok(response)
    }

    /// Send a one-way message without waiting for response.
    ///
    /// Fire-and-forget messaging for cases where the caller doesn't need
    /// to know if the message was processed successfully.
    ///
    /// # Parameters
    ///
    /// - `message`: The message to send (must be serializable)
    ///
    /// # Errors
    ///
    /// - `ActorError::Message` - Serialization error
    /// - `ActorError::NodeUnavailable` - Target node unreachable
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Log event without waiting for confirmation
    /// alice.send(LogEventRequest {
    ///     event: "large_deposit".into(),
    ///     amount: 10000,
    /// }).await?;
    /// ```
    pub async fn send<Msg>(&self, msg: Msg) -> Result<(), ActorError>
    where
        Msg: serde::Serialize,
    {
        // 1. Get message bus reference
        let message_bus = self.message_bus.as_ref().ok_or_else(|| {
            ActorError::ProcessingFailed(
                "ActorRef created without MessageBus (use ActorRuntime::get_actor())".to_string(),
            )
        })?;

        // 2. Serialize message
        let payload = serde_json::to_vec(&msg).map_err(|e| {
            ActorError::Message(crate::error::MessageError::Serialization(e.to_string()))
        })?;

        // 3. Extract method name from type
        let method_name = std::any::type_name::<Msg>()
            .rsplit("::")
            .next()
            .unwrap_or("unknown")
            .to_string();

        // 4. Create sender actor ID (use system actor for now)
        // TODO: In Phase 4, this will be the current actor's ID from ActorContext
        let sender_actor = ActorId::from_string("system::System/root")
            .map_err(|e| ActorError::ProcessingFailed(format!("Invalid sender ID: {}", e)))?;

        // 5. Get sender's node ID from MessageBus
        // Both target_node and sender_node initially set to sender (MessageBus routing will
        // determine actual target location via directory lookup)
        let sender_node = message_bus.node_id().clone();
        let target_node = sender_node.clone(); // Will be updated by routing logic

        // 6. Create one-way message
        let message = Message::oneway(
            self.actor_id.clone(), // target_actor
            sender_actor,          // sender_actor
            target_node,           // target_node
            sender_node,           // sender_node
            method_name,           // method_name
            payload,               // payload
        );

        // 7. Route the message (no response expected)
        message_bus.route_message(message).await?;

        Ok(())
    }
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            actor_id: self.actor_id.clone(),
            message_bus: self.message_bus.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<A: Actor> std::fmt::Debug for ActorRef<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorRef")
            .field("actor_id", &self.actor_id)
            .field("actor_type", &std::any::type_name::<A>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::{ActorId, DeactivationReason};
    use crate::error::ActorError;
    use async_trait::async_trait;

    // Simple test actor type
    struct TestActor {
        actor_id: ActorId,
    }

    #[async_trait(?Send)]
    impl Actor for TestActor {
        type State = ();
        const ACTOR_TYPE: &'static str = "TestActor";

        fn actor_id(&self) -> &ActorId {
            &self.actor_id
        }

        async fn on_activate(&mut self, _state: Option<Self::State>) -> Result<(), ActorError> {
            Ok(())
        }

        async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
            Ok(())
        }
    }

    #[test]
    fn test_actor_ref_creation() {
        let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
        let actor_ref: ActorRef<TestActor> = ActorRef::new(actor_id.clone());

        assert_eq!(actor_ref.actor_id(), &actor_id);
    }

    #[test]
    fn test_actor_ref_clone() {
        let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
        let actor_ref1: ActorRef<TestActor> = ActorRef::new(actor_id.clone());
        let actor_ref2 = actor_ref1.clone();

        assert_eq!(actor_ref1.actor_id(), actor_ref2.actor_id());
    }
}
