//! PingPongActor - Simple test actor for MessageBus simulation testing.
//!
//! This actor demonstrates:
//! - Request-response messaging pattern
//! - State tracking for invariant validation
//! - Integration with simulation framework

use moonpool::actor::ActorState;
use moonpool::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;

/// Request to ping the actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingRequest {
    pub sequence: u64,
}

/// Response from ping actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PongResponse {
    pub sequence: u64,
    pub actor_key: String,
}

/// Internal state for PingPongActor.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PingPongState {
    pub pings_received: u64,
    pub pongs_sent: u64,
}

/// PingPongActor - responds to ping requests with pong responses.
pub struct PingPongActor {
    actor_id: ActorId,
    state: RefCell<PingPongState>,
}

impl PingPongActor {
    pub fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            state: RefCell::new(PingPongState::default()),
        }
    }

    /// Get the actor's key.
    fn key(&self) -> &str {
        &self.actor_id.key
    }
}

// Implement Actor trait
#[async_trait(?Send)]
impl Actor for PingPongActor {
    type State = PingPongState;
    const ACTOR_TYPE: &'static str = "PingPongActor";

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    // Use Random placement for simulation testing
    fn placement_hint() -> PlacementHint {
        PlacementHint::Random
    }

    async fn on_activate(
        &mut self,
        state: ActorState<Self::State>,
    ) -> std::result::Result<(), ActorError> {
        // Load persisted state from ActorState wrapper
        let loaded_state = state.get().clone();
        *self.state.borrow_mut() = loaded_state;

        tracing::debug!(
            actor_id = %self.actor_id,
            key = self.key(),
            "PingPongActor activated"
        );
        Ok(())
    }

    async fn on_deactivate(
        &mut self,
        reason: DeactivationReason,
    ) -> std::result::Result<(), ActorError> {
        tracing::debug!(
            actor_id = %self.actor_id,
            key = self.key(),
            reason = ?reason,
            "PingPongActor deactivating"
        );
        Ok(())
    }

    // Register message handlers for dynamic dispatch
    fn register_handlers<S: moonpool::serialization::Serializer + 'static>(
        registry: &mut HandlerRegistry<Self, S>,
    ) {
        registry.register::<PingRequest, PongResponse>();
    }
}

// Implement MessageHandler for PingRequest
#[async_trait(?Send)]
impl<S: moonpool::serialization::Serializer> MessageHandler<PingRequest, PongResponse, S>
    for PingPongActor
{
    async fn handle(
        &mut self,
        req: PingRequest,
        _ctx: &ActorContext<Self, S>,
    ) -> std::result::Result<PongResponse, ActorError> {
        // Update state
        {
            let mut state = self.state.borrow_mut();
            state.pings_received += 1;
            state.pongs_sent += 1;
        }

        let response = PongResponse {
            sequence: req.sequence,
            actor_key: self.key().to_string(),
        };

        tracing::trace!(
            actor_id = %self.actor_id,
            sequence = req.sequence,
            "PingPongActor processing ping"
        );

        Ok(response)
    }
}

// Extension trait for typed API
#[async_trait(?Send)]
pub trait PingPongActorRef {
    /// Send a ping request and get a pong response.
    async fn ping(&self, sequence: u64) -> std::result::Result<PongResponse, ActorError>;
}

// Implement extension trait for ActorRef<PingPongActor>
#[async_trait(?Send)]
impl PingPongActorRef for ActorRef<PingPongActor> {
    async fn ping(&self, sequence: u64) -> std::result::Result<PongResponse, ActorError> {
        self.call(PingRequest { sequence }).await
    }
}

// Factory for creating PingPongActor instances
pub struct PingPongActorFactory;

#[async_trait(?Send)]
impl ActorFactory for PingPongActorFactory {
    type Actor = PingPongActor;

    async fn create(&self, actor_id: ActorId) -> std::result::Result<Self::Actor, ActorError> {
        tracing::debug!(
            actor_id = %actor_id,
            key = &actor_id.key,
            "Creating PingPongActor instance"
        );
        Ok(PingPongActor::new(actor_id))
    }
}
