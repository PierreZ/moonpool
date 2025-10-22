//! Unit tests for concurrent activation race detection.
//!
//! These tests verify that:
//! - Directory detects concurrent activation attempts
//! - PlacementDecision::Race is returned correctly
//! - Winner/loser nodes are properly identified

use moonpool::actor::{ActorId, NodeId};
use moonpool::directory::{Directory, PlacementDecision, SimpleDirectory};

#[tokio::test]
async fn test_no_race_first_activation() {
    // First activation should always succeed
    let node1 = NodeId::from("127.0.0.1:5000").unwrap();
    let directory = SimpleDirectory::new(vec![node1.clone()]);
    let actor_id = ActorId::from_string("test::TestActor/actor1").unwrap();

    let decision = directory.register(actor_id, node1.clone()).await.unwrap();

    assert!(matches!(decision, PlacementDecision::PlaceOnNode(_)));
    if let PlacementDecision::PlaceOnNode(node) = decision {
        assert_eq!(node, node1);
    }
}

#[tokio::test]
async fn test_race_detection_same_actor() {
    // Registering the same actor from two different nodes should detect a race
    let node1 = NodeId::from("127.0.0.1:5000").unwrap();
    let node2 = NodeId::from("127.0.0.1:5001").unwrap();
    let directory = SimpleDirectory::new(vec![node1.clone(), node2.clone()]);
    let actor_id = ActorId::from_string("test::TestActor/actor2").unwrap();

    // First registration succeeds
    let decision1 = directory
        .register(actor_id.clone(), node1.clone())
        .await
        .unwrap();
    assert!(matches!(decision1, PlacementDecision::PlaceOnNode(_)));

    // Second registration from different node detects race
    let decision2 = directory.register(actor_id, node2.clone()).await.unwrap();

    match decision2 {
        PlacementDecision::AlreadyRegistered(registered_node) => {
            // Already registered on the first node
            assert_eq!(registered_node, node1);
        }
        PlacementDecision::Race { winner, loser } => {
            // Race detected - one won, one lost
            assert!((winner == node1 && loser == node2) || (winner == node2 && loser == node1));
        }
        _ => panic!("Expected AlreadyRegistered or Race"),
    }
}

#[tokio::test]
async fn test_no_race_same_node_reregistration() {
    // Registering the same actor from the same node should be idempotent (succeed)
    let node1 = NodeId::from("127.0.0.1:5000").unwrap();
    let directory = SimpleDirectory::new(vec![node1.clone()]);
    let actor_id = ActorId::from_string("test::TestActor/actor3").unwrap();

    // First registration
    let decision1 = directory
        .register(actor_id.clone(), node1.clone())
        .await
        .unwrap();
    assert!(decision1.is_successful());

    // Second registration from same node (idempotent - should succeed)
    let decision2 = directory.register(actor_id, node1.clone()).await.unwrap();
    assert!(decision2.is_successful());
}

#[tokio::test]
async fn test_multiple_actors_no_interference() {
    // Different actors should not interfere with each other
    let node1 = NodeId::from("127.0.0.1:5000").unwrap();
    let node2 = NodeId::from("127.0.0.1:5001").unwrap();
    let directory = SimpleDirectory::new(vec![node1.clone(), node2.clone()]);
    let actor1 = ActorId::from_string("test::TestActor/actor4").unwrap();
    let actor2 = ActorId::from_string("test::TestActor/actor5").unwrap();

    // Register two different actors on different nodes
    let decision1 = directory.register(actor1, node1).await.unwrap();
    let decision2 = directory.register(actor2, node2).await.unwrap();

    // Both should succeed
    assert!(matches!(decision1, PlacementDecision::PlaceOnNode(_)));
    assert!(matches!(decision2, PlacementDecision::PlaceOnNode(_)));
}

#[tokio::test]
async fn test_lookup_after_registration() {
    // Verify lookup returns correct node after registration
    let node1 = NodeId::from("127.0.0.1:5000").unwrap();
    let directory = SimpleDirectory::new(vec![node1.clone()]);
    let actor_id = ActorId::from_string("test::TestActor/actor6").unwrap();

    // Register actor
    directory
        .register(actor_id.clone(), node1.clone())
        .await
        .unwrap();

    // Lookup should return the registered node
    let lookup_result = directory.lookup(&actor_id).await.unwrap();
    assert!(lookup_result.is_some());
    assert_eq!(lookup_result.unwrap(), node1);
}

#[tokio::test]
async fn test_unregister_allows_new_registration() {
    // After unregistering, a new registration should succeed
    let node1 = NodeId::from("127.0.0.1:5000").unwrap();
    let node2 = NodeId::from("127.0.0.1:5001").unwrap();
    let directory = SimpleDirectory::new(vec![node1.clone(), node2.clone()]);
    let actor_id = ActorId::from_string("test::TestActor/actor7").unwrap();

    // Register, then unregister
    directory.register(actor_id.clone(), node1).await.unwrap();
    directory.unregister(&actor_id).await.unwrap();

    // New registration on different node should succeed
    let decision = directory.register(actor_id, node2.clone()).await.unwrap();
    assert!(matches!(decision, PlacementDecision::PlaceOnNode(_)));

    if let PlacementDecision::PlaceOnNode(node) = decision {
        assert_eq!(node, node2);
    }
}
