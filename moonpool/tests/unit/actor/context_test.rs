//! Unit tests for ActorContext lifecycle.
//!
//! Tests cover:
//! - Context creation and initialization
//! - Lifecycle state transitions
//! - Actor instance management
//! - Node ID and actor ID accessors

use moonpool::actor::{ActorContext, ActorId, ActivationState, DeactivationReason, NodeId};
use moonpool::actor::traits::Actor;
use moonpool::error::ActorError;
use async_trait::async_trait;

/// Simple test actor for context lifecycle testing
struct TestActor {
    actor_id: ActorId,
    activation_count: u32,
    deactivation_count: u32,
}

impl TestActor {
    fn new(actor_id: ActorId) -> Self {
        Self {
            actor_id,
            activation_count: 0,
            deactivation_count: 0,
        }
    }
}

#[async_trait(?Send)]
impl Actor for TestActor {
    type State = ();  // Stateless

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(&mut self, _state: Option<()>) -> Result<(), ActorError> {
        self.activation_count += 1;
        Ok(())
    }

    async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
        self.deactivation_count += 1;
        Ok(())
    }
}

#[test]
fn test_context_creation() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id.clone(), node_id.clone(), actor);

    // Verify initial state
    assert_eq!(context.actor_id, actor_id);
    assert_eq!(context.node_id, node_id);
    assert_eq!(context.get_state(), ActivationState::Creating);
}

#[test]
fn test_state_transition_to_activating() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id, node_id, actor);

    // Transition to Activating
    context.set_state(ActivationState::Activating).unwrap();
    assert_eq!(context.get_state(), ActivationState::Activating);
}

#[tokio::test]
async fn test_activation_lifecycle() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id, node_id, actor);

    // Initial state
    assert_eq!(context.get_state(), ActivationState::Creating);

    // Transition to Activating
    context.set_state(ActivationState::Activating).unwrap();

    // Call on_activate
    {
        let mut actor = context.actor_instance.borrow_mut();
        actor.on_activate(None).await.unwrap();
    }

    // Transition to Valid
    context.set_state(ActivationState::Valid).unwrap();
    assert_eq!(context.get_state(), ActivationState::Valid);

    // Verify activation was called
    {
        let actor = context.actor_instance.borrow();
        assert_eq!(actor.activation_count, 1);
    }
}

#[tokio::test]
async fn test_deactivation_lifecycle() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id, node_id, actor);

    // Setup: activate the actor
    context.set_state(ActivationState::Activating).unwrap();
    {
        let mut actor = context.actor_instance.borrow_mut();
        actor.on_activate(None).await.unwrap();
    }
    context.set_state(ActivationState::Valid).unwrap();

    // Deactivation flow
    context.set_state(ActivationState::Deactivating).unwrap();
    {
        let mut actor = context.actor_instance.borrow_mut();
        actor.on_deactivate(DeactivationReason::IdleTimeout).await.unwrap();
    }
    context.set_state(ActivationState::Invalid).unwrap();

    assert_eq!(context.get_state(), ActivationState::Invalid);

    // Verify deactivation was called
    {
        let actor = context.actor_instance.borrow();
        assert_eq!(actor.deactivation_count, 1);
    }
}

#[test]
fn test_actor_id_accessor() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id.clone(), node_id, actor);

    assert_eq!(context.actor_id, actor_id);
    assert_eq!(context.actor_id.namespace, "test");
    assert_eq!(context.actor_id.actor_type, "TestActor");
    assert_eq!(context.actor_id.key, "1");
}

#[test]
fn test_node_id_accessor() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id, node_id.clone(), actor);

    assert_eq!(context.node_id, node_id);
}

#[test]
fn test_multiple_state_transitions() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id, node_id, actor);

    // Valid state transition sequence
    assert_eq!(context.get_state(), ActivationState::Creating);

    context.set_state(ActivationState::Activating).unwrap();
    assert_eq!(context.get_state(), ActivationState::Activating);

    context.set_state(ActivationState::Valid).unwrap();
    assert_eq!(context.get_state(), ActivationState::Valid);

    context.set_state(ActivationState::Deactivating).unwrap();
    assert_eq!(context.get_state(), ActivationState::Deactivating);

    context.set_state(ActivationState::Invalid).unwrap();
    assert_eq!(context.get_state(), ActivationState::Invalid);
}

#[tokio::test]
async fn test_activation_time_tracking() {
    let actor_id = ActorId::from_string("test::TestActor/1").unwrap();
    let node_id = NodeId::from("127.0.0.1:8001").unwrap();
    let actor = TestActor::new(actor_id.clone());

    let context = ActorContext::new(actor_id, node_id, actor);

    let activation_time = context.activation_time;

    // Small delay
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Activation time should remain constant
    assert_eq!(context.activation_time, activation_time);
}
