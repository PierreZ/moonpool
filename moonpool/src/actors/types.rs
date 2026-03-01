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

impl ActorId {
    /// Create a new actor ID.
    pub fn new(actor_type: ActorType, identity: impl Into<String>) -> Self {
        Self {
            actor_type,
            identity: identity.into(),
        }
    }
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
/// Wraps the serialized response body or an error from the handler.
/// The caller deserializes the body using the expected response type
/// for the method called, or propagates the error.
///
/// May include a `cache_invalidation` hint when the message was forwarded
/// because the caller's directory cache was stale.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ActorResponse {
    /// Serialized method-specific response body, or an error message
    /// from the handler.
    pub body: Result<Vec<u8>, String>,
    /// Optional cache invalidation hint piggybacked on the response.
    /// The caller should update its directory cache when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_invalidation: Option<CacheInvalidation>,
}

/// Cache invalidation hint sent when a message was forwarded.
///
/// When a node receives an `ActorMessage` for an actor it doesn't host,
/// it forwards the message and attaches this hint so the caller can
/// update its directory cache.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct CacheInvalidation {
    /// The actor whose location was stale.
    pub actor_id: ActorId,
    /// The endpoint that was incorrectly cached.
    pub invalid_endpoint: crate::Endpoint,
    /// The correct endpoint (if known after forwarding).
    pub valid_endpoint: Option<crate::Endpoint>,
}

/// Unique identifier for a specific actor activation.
///
/// When an actor is activated on a node, it gets a unique `ActivationId`.
/// If the actor is deactivated and later re-activated (possibly on a
/// different node), it gets a new `ActivationId`. This distinguishes
/// stale directory entries from current ones.
///
/// # Orleans Reference
///
/// Corresponds to Orleans' `ActivationId` — a unique tag generated
/// per activation, used for conflict resolution in the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActivationId(pub u64);

impl ActivationId {
    /// Create a new activation ID from a raw value.
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

impl std::fmt::Display for ActivationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "act-{:016x}", self.0)
    }
}

/// Full address of an actor activation: identity + location + activation.
///
/// This is the value stored in the directory. It tells you not just
/// *which* actor and *where* it is, but *which specific activation*
/// is registered. This allows safe re-registration after deactivation.
///
/// # Orleans Reference
///
/// Corresponds to Orleans' `GrainAddress` which contains:
/// - `GrainId` (our `ActorId`)
/// - `SiloAddress` (our `NetworkAddress` in the `Endpoint`)
/// - `ActivationId` (our `ActivationId`)
///
/// The key insight from Orleans: `Register()` returns the existing
/// `GrainAddress` on conflict. The caller can compare activation IDs
/// to detect stale registrations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorAddress {
    /// The actor's identity (type + key).
    pub actor_id: ActorId,
    /// The endpoint where this activation is hosted.
    pub endpoint: crate::Endpoint,
    /// Unique identifier for this specific activation.
    pub activation_id: ActivationId,
}

impl ActorAddress {
    /// Create a new actor address.
    pub fn new(actor_id: ActorId, endpoint: crate::Endpoint, activation_id: ActivationId) -> Self {
        Self {
            actor_id,
            endpoint,
            activation_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

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
        let deserialized: ActorMessage = serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(deserialized.target.identity, "account-123");
        assert_eq!(
            deserialized.sender.as_ref().map(|s| &s.identity[..]),
            Some("player-42")
        );
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
        let deserialized: ActorMessage = serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(deserialized.target.identity, "account-456");
        assert!(deserialized.sender.is_none());
        assert_eq!(deserialized.method, 3);
        assert!(deserialized.body.is_empty());
        assert_eq!(deserialized.forward_count, 2);
    }

    #[test]
    fn test_actor_response_serialization_roundtrip() {
        let resp = ActorResponse {
            body: Ok(vec![10, 20, 30]),
            cache_invalidation: None,
        };

        let serialized = serde_json::to_vec(&resp).expect("serialize");
        let deserialized: ActorResponse = serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(deserialized.body, Ok(vec![10, 20, 30]));
    }

    #[test]
    fn test_actor_response_error_roundtrip() {
        let resp = ActorResponse {
            body: Err("unknown method: 99".to_string()),
            cache_invalidation: None,
        };

        let serialized = serde_json::to_vec(&resp).expect("serialize");
        let deserialized: ActorResponse = serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(deserialized.body, Err("unknown method: 99".to_string()));
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

    #[test]
    fn test_activation_id_display() {
        let id = ActivationId::new(42);
        assert_eq!(format!("{id}"), "act-000000000000002a");
    }

    #[test]
    fn test_actor_address_equality() {
        let endpoint = crate::Endpoint::new(
            crate::NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500),
            crate::UID::new(0xBA4E_4B00, 0),
        );
        let actor_id = ActorId::new(ActorType(0xBA4E_4B00), "alice");

        let addr1 = ActorAddress::new(actor_id.clone(), endpoint.clone(), ActivationId::new(1));
        let addr2 = ActorAddress::new(actor_id.clone(), endpoint.clone(), ActivationId::new(1));
        assert_eq!(addr1, addr2);
    }

    #[test]
    fn test_actor_address_different_activation_not_equal() {
        let endpoint = crate::Endpoint::new(
            crate::NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500),
            crate::UID::new(0xBA4E_4B00, 0),
        );
        let actor_id = ActorId::new(ActorType(0xBA4E_4B00), "alice");

        let addr1 = ActorAddress::new(actor_id.clone(), endpoint.clone(), ActivationId::new(1));
        let addr2 = ActorAddress::new(actor_id, endpoint, ActivationId::new(2));
        assert_ne!(addr1, addr2);
    }

    #[test]
    fn test_actor_address_serialization_roundtrip() {
        let endpoint = crate::Endpoint::new(
            crate::NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500),
            crate::UID::new(0xBA4E_4B00, 0),
        );
        let actor_id = ActorId::new(ActorType(0xBA4E_4B00), "alice");
        let addr = ActorAddress::new(actor_id, endpoint, ActivationId::new(99));

        let serialized = serde_json::to_vec(&addr).expect("serialize");
        let deserialized: ActorAddress = serde_json::from_slice(&serialized).expect("deserialize");

        assert_eq!(addr, deserialized);
    }
}
