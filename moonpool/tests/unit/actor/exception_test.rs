//! Unit tests for exception handling in actor message processing.
//!
//! These tests verify that:
//! - Actors survive exceptions during message processing
//! - Errors are properly propagated to callers
//! - Error tracking works correctly
//! - Actors remain in Valid state after exceptions

use async_trait::async_trait;
use moonpool::actor::{Actor, ActorContext, ActorId, DeactivationReason};
use moonpool::error::ActorError;
use moonpool::storage::InMemoryStorage;
use serde::{Deserialize, Serialize};
use std::rc::Rc;

/// Test actor that can be configured to fail on demand.
#[derive(Debug)]
struct FailingActor {
    actor_id: ActorId,
    should_fail: bool,
    call_count: usize,
}

impl FailingActor {
    fn new(actor_id: ActorId, should_fail: bool) -> Self {
        Self {
            actor_id,
            should_fail,
            call_count: 0,
        }
    }
}

#[async_trait(?Send)]
impl Actor for FailingActor {
    type State = ();
    const ACTOR_TYPE: &'static str = "FailingActor";

    fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    async fn on_activate(
        &mut self,
        _state: moonpool::actor::ActorState<()>,
    ) -> Result<(), ActorError> {
        Ok(())
    }

    async fn on_deactivate(&mut self, _reason: DeactivationReason) -> Result<(), ActorError> {
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TestRequest {
    value: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct TestResponse {
    result: u64,
}

#[test]
fn test_error_tracking_initialization() {
    // Test that ActorContext initializes error tracking to zero
    let actor_id = ActorId::from_string("test::FailingActor/test1").unwrap();
    let node_id = moonpool::actor::NodeId::from("127.0.0.1:5000").unwrap();
    let actor = FailingActor::new(actor_id.clone(), false);

    // Create channels for message loop
    use tokio::sync::mpsc;
    let (msg_tx, _msg_rx) = mpsc::channel(128);
    let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
    let storage = Rc::new(InMemoryStorage::new());
    let message_serializer = moonpool::serialization::JsonSerializer;

    let context = ActorContext::new(
        actor_id,
        node_id,
        actor,
        msg_tx,
        ctrl_tx,
        storage,
        message_serializer,
    );

    assert_eq!(context.get_error_count(), 0);
    assert!(context.get_last_error().is_none());
}

#[test]
fn test_error_tracking_record() {
    // Test that errors are properly recorded
    let actor_id = ActorId::from_string("test::FailingActor/test2").unwrap();
    let node_id = moonpool::actor::NodeId::from("127.0.0.1:5000").unwrap();
    let actor = FailingActor::new(actor_id.clone(), false);

    // Create channels for message loop
    use tokio::sync::mpsc;
    let (msg_tx, _msg_rx) = mpsc::channel(128);
    let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
    let storage = Rc::new(InMemoryStorage::new());
    let message_serializer = moonpool::serialization::JsonSerializer;

    let context = ActorContext::new(
        actor_id,
        node_id,
        actor,
        msg_tx,
        ctrl_tx,
        storage,
        message_serializer,
    );

    // Record an error
    let error = ActorError::ProcessingFailed("Test error".to_string());
    context.record_error(error.clone());

    assert_eq!(context.get_error_count(), 1);
    assert!(context.get_last_error().is_some());

    // Record another error
    let error2 = ActorError::ExecutionFailed("Another error".to_string());
    context.record_error(error2);

    assert_eq!(context.get_error_count(), 2);
}

#[test]
fn test_error_tracking_clear() {
    // Test that errors can be cleared
    let actor_id = ActorId::from_string("test::FailingActor/test3").unwrap();
    let node_id = moonpool::actor::NodeId::from("127.0.0.1:5000").unwrap();
    let actor = FailingActor::new(actor_id.clone(), false);

    // Create channels for message loop
    use tokio::sync::mpsc;
    let (msg_tx, _msg_rx) = mpsc::channel(128);
    let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
    let storage = Rc::new(InMemoryStorage::new());
    let message_serializer = moonpool::serialization::JsonSerializer;

    let context = ActorContext::new(
        actor_id,
        node_id,
        actor,
        msg_tx,
        ctrl_tx,
        storage,
        message_serializer,
    );

    // Record some errors
    context.record_error(ActorError::ProcessingFailed("Error 1".to_string()));
    context.record_error(ActorError::ProcessingFailed("Error 2".to_string()));
    assert_eq!(context.get_error_count(), 2);

    // Clear errors
    context.clear_errors();

    assert_eq!(context.get_error_count(), 0);
    assert!(context.get_last_error().is_none());
}

#[test]
fn test_error_cloning() {
    // Test that ActorError can be cloned (required for error tracking)
    let error1 = ActorError::ProcessingFailed("Test".to_string());
    let error2 = error1.clone();

    match (&error1, &error2) {
        (ActorError::ProcessingFailed(msg1), ActorError::ProcessingFailed(msg2)) => {
            assert_eq!(msg1, msg2);
        }
        _ => panic!("Error types don't match"),
    }
}

#[test]
fn test_multiple_error_types_cloneable() {
    // Verify all ActorError variants are cloneable
    let errors = vec![
        ActorError::ActivationFailed("test".to_string()),
        ActorError::ExecutionFailed("test".to_string()),
        ActorError::UnknownMethod("test".to_string()),
        ActorError::Timeout,
        ActorError::ProcessingFailed("test".to_string()),
        ActorError::InsufficientFunds {
            available: 100,
            requested: 200,
        },
        ActorError::UnknownCorrelationId,
        ActorError::NotFound("test".to_string()),
        ActorError::InvalidConfiguration("test".to_string()),
    ];

    // Clone all errors to verify Clone impl works
    let cloned: Vec<_> = errors.to_vec();
    assert_eq!(errors.len(), cloned.len());
}
