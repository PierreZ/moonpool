//! HelloActor - Simple actor demonstrating the extension trait pattern.
//!
//! This actor responds with "Hello from {key}!" to demonstrate:
//! - Clean typed API via extension traits
//! - Automatic actor activation
//! - Location-transparent messaging

use moonpool::prelude::*;
use serde::{Deserialize, Serialize};

/// Request to say hello.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SayHelloRequest;

/// HelloActor - responds with personalized greeting.
pub struct HelloActor {
    actor_id: ActorId,
}

impl HelloActor {
    pub fn new(actor_id: ActorId) -> Self {
        Self { actor_id }
    }

    /// Get the actor's key (e.g., "alice", "bob").
    fn key(&self) -> &str {
        &self.actor_id.key
    }
}

// Implement Actor trait
#[async_trait(?Send)]
impl Actor for HelloActor {
    type State = ();
    const ACTOR_TYPE: &'static str = "HelloActor";

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    // Use LeastLoaded placement for automatic load balancing across nodes
    fn placement_hint() -> PlacementHint {
        PlacementHint::LeastLoaded
    }

    async fn on_activate(&mut self, _state: Option<()>) -> std::result::Result<(), ActorError> {
        tracing::info!(
            actor_id = %self.actor_id,
            key = self.key(),
            "HelloActor activated"
        );
        Ok(())
    }

    async fn on_deactivate(
        &mut self,
        reason: DeactivationReason,
    ) -> std::result::Result<(), ActorError> {
        tracing::info!(
            actor_id = %self.actor_id,
            key = self.key(),
            reason = ?reason,
            "HelloActor deactivating"
        );
        Ok(())
    }

    // Register message handlers for dynamic dispatch
    fn register_handlers(registry: &mut HandlerRegistry<Self>) {
        registry.register::<SayHelloRequest, String>();
    }
}

// Implement MessageHandler for SayHelloRequest
#[async_trait(?Send)]
impl MessageHandler<SayHelloRequest, String> for HelloActor {
    async fn handle(
        &mut self,
        _req: SayHelloRequest,
        _ctx: &ActorContext<Self>,
    ) -> std::result::Result<String, ActorError> {
        let greeting = format!("Hello from {}!", self.key());
        tracing::debug!(
            actor_id = %self.actor_id,
            greeting = %greeting,
            "Processing SayHelloRequest"
        );
        Ok(greeting)
    }
}

// Extension trait for typed API
#[async_trait(?Send)]
pub trait HelloActorRef {
    /// Say hello and get a personalized greeting.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let alice = runtime.get_actor::<HelloActor>("HelloActor", "alice")?;
    /// let greeting = alice.say_hello().await?;
    /// println!("{}", greeting); // "Hello from alice!"
    /// ```
    async fn say_hello(&self) -> std::result::Result<String, ActorError>;
}

// Implement extension trait for ActorRef<HelloActor>
#[async_trait(?Send)]
impl HelloActorRef for ActorRef<HelloActor> {
    async fn say_hello(&self) -> std::result::Result<String, ActorError> {
        self.call(SayHelloRequest).await
    }
}

// Factory for creating HelloActor instances
pub struct HelloActorFactory;

#[async_trait(?Send)]
impl ActorFactory for HelloActorFactory {
    type Actor = HelloActor;

    async fn create(&self, actor_id: ActorId) -> std::result::Result<Self::Actor, ActorError> {
        tracing::info!(
            actor_id = %actor_id,
            key = &actor_id.key,
            "Creating HelloActor instance"
        );
        Ok(HelloActor::new(actor_id))
    }
}
