//! Core virtual actor types.
//!
//! These types form the messaging contract between actor callers and handlers.
//! The transport layer treats `ActorMessage` and `ActorResponse` as opaque
//! payloads — they're wrapped in `RequestEnvelope` by `send_request()` just
//! like any other RPC message.
//!
//! # Design
//!
//! - `ActorType` is a u64 derived from the interface/trait name.
//! - `ActorId` is `ActorType` + a string identity (e.g., "player-42").
//! - `ActorMessage` carries the target identity, method discriminant, and
//!   serialized body. The transport never knows which method is being called.
//! - `ActorResponse` wraps the serialized response body.
//!
//! # FDB/Orleans References
//!
//! This follows Orleans' GrainReference pattern: the caller builds an
//! `ActorMessage` with identity + method + body, sends it to the actor
//! type's dispatch token (index 0), and the handler routes by identity.

use serde::{Deserialize, Serialize};

/// Identifies an actor TYPE — derived from the trait/interface name.
///
/// This is a stable identifier for a class of actors, not a specific
/// instance. For example, `PlayerActor` and `BankAccount` each have
/// their own `ActorType`.
///
/// # Convention
///
/// Use a hex constant matching the interface ID pattern:
/// ```rust
/// use moonpool::actors::ActorType;
/// const BANK_ACCOUNT: ActorType = ActorType(0xBA4E_4B00);
/// ```
#[derive(Clone, Copy, Hash, Eq, PartialEq, Serialize, Deserialize, Debug)]
pub struct ActorType(pub u64);

/// Full virtual actor address = type + string identity.
///
/// Uniquely identifies a specific actor instance within the system.
///
/// # Examples
///
/// ```rust
/// use moonpool::actors::{ActorId, ActorType};
///
/// let player = ActorId {
///     actor_type: ActorType(0x504C_4159),
///     identity: "player-42".to_string(),
/// };
/// ```
#[derive(Clone, Hash, Eq, PartialEq, Serialize, Deserialize, Debug)]
pub struct ActorId {
    /// The type of actor (identifies the handler).
    pub actor_type: ActorType,
    /// The specific instance identity (e.g., "player-42", "account-abc").
    pub identity: String,
}

/// Message payload for virtual actor calls.
///
/// Wrapped in `RequestEnvelope` by `send_request()` — the transport treats
/// it like any other request. Method dispatch is in the payload, not the
/// transport token.
///
/// # Wire Format
///
/// ```text
/// RequestEnvelope {
///     request: ActorMessage { target, sender, method, body, forward_count },
///     reply_to: Endpoint,
/// }
/// ```
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ActorMessage {
    /// The target actor instance.
    pub target: ActorId,
    /// Optional sender actor (for actor-to-actor calls).
    pub sender: Option<ActorId>,
    /// Method discriminant within the actor type (1, 2, 3, …).
    pub method: u32,
    /// Serialized method-specific request body.
    pub body: Vec<u8>,
    /// Number of times this message has been forwarded (prevents loops).
    pub forward_count: u8,
}

/// Response from a virtual actor.
///
/// Wraps the serialized response body. The caller deserializes
/// the body using the expected response type for the method called.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ActorResponse {
    /// Serialized method-specific response body.
    pub body: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_actor_type_equality() {
        let t1 = ActorType(0xBA4E_4B00);
        let t2 = ActorType(0xBA4E_4B00);
        let t3 = ActorType(0x504C_4159);

        assert_eq!(t1, t2);
        assert_ne!(t1, t3);
    }

    #[test]
    fn test_actor_id_equality() {
        let id1 = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "alice".to_string(),
        };
        let id2 = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "alice".to_string(),
        };
        let id3 = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "bob".to_string(),
        };

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_actor_id_different_types_not_equal() {
        let id1 = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "alice".to_string(),
        };
        let id2 = ActorId {
            actor_type: ActorType(0x504C_4159),
            identity: "alice".to_string(),
        };

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_actor_message_serialization_roundtrip() {
        let msg = ActorMessage {
            target: ActorId {
                actor_type: ActorType(0xBA4E_4B00),
                identity: "account-123".to_string(),
            },
            sender: Some(ActorId {
                actor_type: ActorType(0x504C_4159),
                identity: "player-42".to_string(),
            }),
            method: 1,
            body: vec![1, 2, 3, 4],
            forward_count: 0,
        };

        let serialized = serde_json::to_vec(&msg).expect("serialize");
        let deserialized: ActorMessage =
            serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(deserialized.target.identity, "account-123");
        assert_eq!(deserialized.sender.as_ref().map(|s| &s.identity[..]), Some("player-42"));
        assert_eq!(deserialized.method, 1);
        assert_eq!(deserialized.body, vec![1, 2, 3, 4]);
        assert_eq!(deserialized.forward_count, 0);
    }

    #[test]
    fn test_actor_message_no_sender_roundtrip() {
        let msg = ActorMessage {
            target: ActorId {
                actor_type: ActorType(0xBA4E_4B00),
                identity: "account-456".to_string(),
            },
            sender: None,
            method: 3,
            body: vec![],
            forward_count: 2,
        };

        let serialized = serde_json::to_vec(&msg).expect("serialize");
        let deserialized: ActorMessage =
            serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(deserialized.target.identity, "account-456");
        assert!(deserialized.sender.is_none());
        assert_eq!(deserialized.method, 3);
        assert!(deserialized.body.is_empty());
        assert_eq!(deserialized.forward_count, 2);
    }

    #[test]
    fn test_actor_response_serialization_roundtrip() {
        let resp = ActorResponse {
            body: vec![10, 20, 30],
        };

        let serialized = serde_json::to_vec(&resp).expect("serialize");
        let deserialized: ActorResponse =
            serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(deserialized.body, vec![10, 20, 30]);
    }

    #[test]
    fn test_actor_type_hash_works_in_collections() {
        use std::collections::HashMap;

        let mut map = HashMap::new();
        map.insert(ActorType(0xBA4E_4B00), "BankAccount");
        map.insert(ActorType(0x504C_4159), "Player");

        assert_eq!(map.get(&ActorType(0xBA4E_4B00)), Some(&"BankAccount"));
        assert_eq!(map.get(&ActorType(0x504C_4159)), Some(&"Player"));
    }

    #[test]
    fn test_actor_id_hash_works_in_collections() {
        use std::collections::HashMap;

        let mut map = HashMap::new();
        let alice = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "alice".to_string(),
        };
        let bob = ActorId {
            actor_type: ActorType(0xBA4E_4B00),
            identity: "bob".to_string(),
        };

        map.insert(alice.clone(), 100);
        map.insert(bob.clone(), 50);

        assert_eq!(map.get(&alice), Some(&100));
        assert_eq!(map.get(&bob), Some(&50));
    }
}
