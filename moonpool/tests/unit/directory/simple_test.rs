//! Unit tests for SimpleDirectory operations.
//!
//! Tests cover:
//! - Basic lookup/register/unregister operations
//! - Race detection for concurrent registrations
//! - Node load tracking and updates
//! - Placement algorithm behavior
//! - Idempotent operations
//! - Storage key isolation

use moonpool::actor::{ActorId, NodeId};
use moonpool::directory::{Directory, PlacementDecision, SimpleDirectory};

#[tokio::test]
async fn test_directory_creation() {
    let _nodes = [
        NodeId::from("127.0.0.1:8001").unwrap(),
        NodeId::from("127.0.0.1:8002").unwrap(),
        NodeId::from("127.0.0.1:8003").unwrap(),
    ];
    let directory = SimpleDirectory::new();

    // Verify directory is created successfully
    let actor_id = ActorId::from_string("test::Counter/alice").unwrap();
    let result = directory.lookup(&actor_id).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), None);
}

#[tokio::test]
async fn test_basic_lookup_register_unregister() {
    let nodes = [
        NodeId::from("127.0.0.1:8001").unwrap(),
        NodeId::from("127.0.0.1:8002").unwrap(),
    ];
    let directory = SimpleDirectory::new();
    let actor_id = ActorId::from_string("test::Counter/bob").unwrap();

    // Lookup non-existent actor
    let location = directory.lookup(&actor_id).await.unwrap();
    assert_eq!(location, None);

    // Register actor on node 0
    let decision = directory
        .register(actor_id.clone(), nodes[0].clone())
        .await
        .unwrap();

    assert!(matches!(decision, PlacementDecision::PlaceOnNode(_)));
    if let PlacementDecision::PlaceOnNode(node) = decision {
        assert_eq!(node, nodes[0]);
    }

    // Lookup registered actor
    let location = directory.lookup(&actor_id).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));

    // Unregister actor
    directory.unregister(&actor_id).await.unwrap();

    // Lookup after unregister
    let location = directory.lookup(&actor_id).await.unwrap();
    assert_eq!(location, None);
}

#[tokio::test]
async fn test_registration_race_detection() {
    let nodes = [
        NodeId::from("127.0.0.1:8001").unwrap(),
        NodeId::from("127.0.0.1:8002").unwrap(),
        NodeId::from("127.0.0.1:8003").unwrap(),
    ];
    let directory = SimpleDirectory::new();
    let actor_id = ActorId::from_string("test::BankAccount/charlie").unwrap();

    // Node 0 registers first (winner)
    let decision1 = directory
        .register(actor_id.clone(), nodes[0].clone())
        .await
        .unwrap();
    assert!(matches!(decision1, PlacementDecision::PlaceOnNode(_)));

    // Node 1 tries to register same actor (race - loser)
    let decision2 = directory
        .register(actor_id.clone(), nodes[1].clone())
        .await
        .unwrap();

    assert!(matches!(decision2, PlacementDecision::Race { .. }));
    if let PlacementDecision::Race { winner, loser } = decision2 {
        assert_eq!(winner, nodes[0]);
        assert_eq!(loser, nodes[1]);
    }

    // Node 2 also tries (also loses)
    let decision3 = directory
        .register(actor_id.clone(), nodes[2].clone())
        .await
        .unwrap();

    assert!(matches!(decision3, PlacementDecision::Race { .. }));
    if let PlacementDecision::Race { winner, loser } = decision3 {
        assert_eq!(winner, nodes[0]);
        assert_eq!(loser, nodes[2]);
    }

    // Verify actor is still on node 0
    let location = directory.lookup(&actor_id).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
}

#[tokio::test]
async fn test_idempotent_register() {
    let nodes = [NodeId::from("127.0.0.1:8001").unwrap()];
    let directory = SimpleDirectory::new();
    let actor_id = ActorId::from_string("test::Counter/dave").unwrap();

    // Register actor
    let decision1 = directory
        .register(actor_id.clone(), nodes[0].clone())
        .await
        .unwrap();
    assert!(decision1.is_successful());

    // Register again on same node (idempotent)
    let decision2 = directory
        .register(actor_id.clone(), nodes[0].clone())
        .await
        .unwrap();
    assert!(decision2.is_successful());

    // Should still be registered
    let location = directory.lookup(&actor_id).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
}

#[tokio::test]
async fn test_idempotent_unregister() {
    let nodes = [NodeId::from("127.0.0.1:8001").unwrap()];
    let directory = SimpleDirectory::new();
    let actor_id = ActorId::from_string("test::Counter/eve").unwrap();

    // Register actor
    directory
        .register(actor_id.clone(), nodes[0].clone())
        .await
        .unwrap();

    // Unregister once
    let result1 = directory.unregister(&actor_id).await;
    assert!(result1.is_ok());

    // Unregister again (idempotent)
    let result2 = directory.unregister(&actor_id).await;
    assert!(result2.is_ok());

    // Should still be unregistered
    let location = directory.lookup(&actor_id).await.unwrap();
    assert_eq!(location, None);
}

#[tokio::test]
async fn test_storage_key_isolation_namespaces() {
    let nodes = [NodeId::from("127.0.0.1:8001").unwrap()];
    let directory = SimpleDirectory::new();

    // Same actor_type and key, different namespaces
    let prod_actor = ActorId::from_string("prod::Counter/frank").unwrap();
    let staging_actor = ActorId::from_string("staging::Counter/frank").unwrap();

    // Register both
    directory
        .register(prod_actor.clone(), nodes[0].clone())
        .await
        .unwrap();
    directory
        .register(staging_actor.clone(), nodes[0].clone())
        .await
        .unwrap();

    // Both should be registered independently
    let location = directory.lookup(&prod_actor).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
    let location = directory.lookup(&staging_actor).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));

    // Unregister prod shouldn't affect staging
    directory.unregister(&prod_actor).await.unwrap();
    assert_eq!(directory.lookup(&prod_actor).await.unwrap(), None);
    let location = directory.lookup(&staging_actor).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
}

#[tokio::test]
async fn test_storage_key_isolation_actor_types() {
    let nodes = [NodeId::from("127.0.0.1:8001").unwrap()];
    let directory = SimpleDirectory::new();

    // Same namespace and key, different actor_types
    let counter = ActorId::from_string("prod::Counter/gina").unwrap();
    let account = ActorId::from_string("prod::BankAccount/gina").unwrap();

    // Register both
    directory
        .register(counter.clone(), nodes[0].clone())
        .await
        .unwrap();
    directory
        .register(account.clone(), nodes[0].clone())
        .await
        .unwrap();

    // Both should be registered independently
    let location = directory.lookup(&counter).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
    let location = directory.lookup(&account).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));

    // Unregister counter shouldn't affect account
    directory.unregister(&counter).await.unwrap();
    assert_eq!(directory.lookup(&counter).await.unwrap(), None);
    let location = directory.lookup(&account).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
}

#[tokio::test]
async fn test_multiple_actors_different_keys() {
    let nodes = [
        NodeId::from("127.0.0.1:8001").unwrap(),
        NodeId::from("127.0.0.1:8002").unwrap(),
    ];
    let directory = SimpleDirectory::new();

    // Register multiple actors with different keys
    let actor1 = ActorId::from_string("prod::Counter/user1").unwrap();
    let actor2 = ActorId::from_string("prod::Counter/user2").unwrap();
    let actor3 = ActorId::from_string("prod::Counter/user3").unwrap();

    directory
        .register(actor1.clone(), nodes[0].clone())
        .await
        .unwrap();
    directory
        .register(actor2.clone(), nodes[1].clone())
        .await
        .unwrap();
    directory
        .register(actor3.clone(), nodes[0].clone())
        .await
        .unwrap();

    // All should be independently registered
    let location = directory.lookup(&actor1).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
    let location = directory.lookup(&actor2).await.unwrap();
    assert_eq!(location, Some(nodes[1].clone()));
    let location = directory.lookup(&actor3).await.unwrap();
    assert_eq!(location, Some(nodes[0].clone()));
}

// NOTE: The following placement tests have been moved to placement module tests.
// Directory now only tracks WHERE actors are, not WHERE they should be placed.
// See tests/unit/placement/ for placement strategy tests.

// #[tokio::test]
// async fn test_placement_algorithm_single_node() - moved to placement tests
// #[tokio::test]
// async fn test_placement_algorithm_multiple_nodes() - moved to placement tests
// #[tokio::test]
// async fn test_empty_cluster_error() - moved to placement tests
