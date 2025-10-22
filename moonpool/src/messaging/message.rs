//! Message types and enums for actor communication.

use crate::actor::{ActorId, CorrelationId, NodeId};
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Message flow semantics and response expectations.
///
/// # State Transitions
///
/// ```text
/// Request → Response  (matching correlation_id)
/// OneWay → (terminal, no response)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Request message expecting a response.
    ///
    /// # Semantics
    ///
    /// - Creates `CallbackData` for response tracking
    /// - Has timeout enforcement (`time_to_expiry` required)
    /// - Response must copy correlation ID
    /// - Blocks caller until response or timeout
    Request,

    /// Response message to a previous request.
    ///
    /// # Semantics
    ///
    /// - Matches request via correlation ID
    /// - Completes pending `CallbackData`
    /// - Does not have timeout (inherits from request)
    /// - Unblocks waiting caller
    Response,

    /// One-way message with no response expected.
    ///
    /// # Semantics
    ///
    /// - No `CallbackData` created
    /// - No timeout enforcement
    /// - Fire-and-forget (caller doesn't wait)
    /// - Best-effort delivery
    OneWay,
}

bitflags! {
    /// Control flags for message processing behavior.
    ///
    /// # Flags
    ///
    /// - `READ_ONLY`: Message doesn't mutate actor state
    /// - `ALWAYS_INTERLEAVE`: Can execute concurrently with other messages
    /// - `IS_LOCAL_ONLY`: Cannot be forwarded to another node
    /// - `SUPPRESS_KEEP_ALIVE`: Don't extend actor idle timeout
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MessageFlags: u16 {
        /// Message doesn't mutate state.
        const READ_ONLY = 1 << 0;

        /// Can execute concurrently (implies READ_ONLY conceptually).
        const ALWAYS_INTERLEAVE = 1 << 1;

        /// Cannot forward to another node.
        const IS_LOCAL_ONLY = 1 << 2;

        /// Don't extend actor lifetime.
        const SUPPRESS_KEEP_ALIVE = 1 << 3;
    }
}

// Manual Serialize/Deserialize for MessageFlags
impl Serialize for MessageFlags {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.bits().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MessageFlags {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bits = u16::deserialize(deserializer)?;
        Ok(MessageFlags::from_bits_truncate(bits))
    }
}

/// Unit of communication between actors.
///
/// # Structure
///
/// Contains addressing, correlation, payload, and control flags for routing
/// and processing messages across the distributed actor system.
///
/// # Validation Rules
///
/// - Request direction MUST have `time_to_expiry`
/// - Response `correlation_id` MUST match pending request
/// - OneWay MUST NOT have response expectation
/// - `forward_count` MUST NOT exceed MAX_FORWARD_COUNT (default: 2)
///
/// # Invariants
///
/// - Response swaps target/sender from request
/// - Response inherits correlation_id from request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// For matching responses to requests.
    pub correlation_id: CorrelationId,

    /// Request, Response, or OneWay.
    pub direction: Direction,

    /// Destination actor ID.
    pub target_actor: ActorId,

    /// Source actor ID.
    pub sender_actor: ActorId,

    /// Physical node hosting target.
    pub target_node: NodeId,

    /// Physical node hosting sender.
    pub sender_node: NodeId,

    /// Method name being invoked (e.g., "DepositRequest").
    pub method_name: String,

    /// Application data (JSON serialized).
    pub payload: Vec<u8>,

    /// Control flags (ReadOnly, AlwaysInterleave, etc.).
    pub flags: MessageFlags,

    /// Optional timeout for message delivery (not serialized - computed on sender).
    #[serde(skip)]
    pub time_to_expiry: Option<Instant>,

    /// Number of times message has been forwarded (prevents loops).
    pub forward_count: u8,

    /// Optional cache invalidation data.
    pub cache_invalidation: Option<Vec<CacheUpdate>>,
}

impl Message {
    /// Create a new request message.
    ///
    /// # Arguments
    ///
    /// - `correlation_id`: Unique ID for matching response
    /// - `target_actor`: Destination actor
    /// - `sender_actor`: Source actor
    /// - `target_node`: Physical node hosting target
    /// - `sender_node`: Physical node hosting sender
    /// - `method_name`: Method being invoked
    /// - `payload`: Serialized request data
    /// - `timeout`: Request timeout
    pub fn request(
        correlation_id: CorrelationId,
        target_actor: ActorId,
        sender_actor: ActorId,
        target_node: NodeId,
        sender_node: NodeId,
        method_name: String,
        payload: Vec<u8>,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            correlation_id,
            direction: Direction::Request,
            target_actor,
            sender_actor,
            target_node,
            sender_node,
            method_name,
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: Some(Instant::now() + timeout),
            forward_count: 0,
            cache_invalidation: None,
        }
    }

    /// Create a response message from a request.
    ///
    /// Automatically swaps target/sender and copies correlation ID.
    pub fn response(request: &Message, payload: Vec<u8>) -> Self {
        Self {
            correlation_id: request.correlation_id,
            direction: Direction::Response,
            target_actor: request.sender_actor.clone(),
            sender_actor: request.target_actor.clone(),
            target_node: request.sender_node.clone(),
            sender_node: request.target_node.clone(),
            method_name: request.method_name.clone(),
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: None,
            forward_count: 0,
            cache_invalidation: None,
        }
    }

    /// Create a one-way message (fire-and-forget).
    pub fn oneway(
        target_actor: ActorId,
        sender_actor: ActorId,
        target_node: NodeId,
        sender_node: NodeId,
        method_name: String,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            correlation_id: CorrelationId::new(0), // Not used for OneWay
            direction: Direction::OneWay,
            target_actor,
            sender_actor,
            target_node,
            sender_node,
            method_name,
            payload,
            flags: MessageFlags::empty(),
            time_to_expiry: None,
            forward_count: 0,
            cache_invalidation: None,
        }
    }

    /// Check if message has timed out.
    pub fn is_timed_out(&self) -> bool {
        self.time_to_expiry
            .map(|expiry| Instant::now() > expiry)
            .unwrap_or(false)
    }

    /// Increment forward count for message forwarding.
    ///
    /// Returns error if exceeds maximum forward count.
    pub fn increment_forward_count(&mut self, max_forwards: u8) -> Result<(), String> {
        if self.forward_count >= max_forwards {
            return Err(format!(
                "Message exceeded maximum forward count: {}",
                max_forwards
            ));
        }
        self.forward_count += 1;
        Ok(())
    }
}

/// Cache update information piggybacked on response messages.
///
/// Used to invalidate stale directory cache entries when actor location changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheUpdate {
    /// Address that is no longer valid.
    pub invalid_address: ActorAddress,

    /// New valid address (None if actor deactivated).
    pub valid_address: Option<ActorAddress>,
}

/// Complete location information for an actor.
///
/// # Structure
///
/// - `actor_id`: Virtual actor identifier
/// - `node_id`: Physical node hosting actor
/// - `activation_time`: When actor was activated (for cache staleness detection, not serialized)
///
/// # Validation Rules
///
/// - `node_id` MUST be valid cluster member
/// - `activation_time` MUST be monotonically increasing per actor_id
///
/// # Invariants
///
/// - Same actor_id CAN have different addresses over time (migration/reactivation)
/// - Same actor_id MUST NOT have multiple addresses simultaneously (single activation)
#[derive(Debug, Clone)]
pub struct ActorAddress {
    pub actor_id: ActorId,
    pub node_id: NodeId,
    pub activation_time: Instant,
}

// Manual Serialize implementation (skip activation_time)
impl serde::Serialize for ActorAddress {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ActorAddress", 2)?;
        state.serialize_field("actor_id", &self.actor_id)?;
        state.serialize_field("node_id", &self.node_id)?;
        state.end()
    }
}

// Manual Deserialize implementation (activation_time set to now)
impl<'de> serde::Deserialize<'de> for ActorAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            ActorId,
            NodeId,
        }

        struct ActorAddressVisitor;

        impl<'de> serde::de::Visitor<'de> for ActorAddressVisitor {
            type Value = ActorAddress;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("struct ActorAddress")
            }

            fn visit_map<V>(self, mut map: V) -> Result<ActorAddress, V::Error>
            where
                V: serde::de::MapAccess<'de>,
            {
                let mut actor_id = None;
                let mut node_id = None;

                while let Some(key) = map.next_key()? {
                    match key {
                        Field::ActorId => {
                            if actor_id.is_some() {
                                return Err(serde::de::Error::duplicate_field("actor_id"));
                            }
                            actor_id = Some(map.next_value()?);
                        }
                        Field::NodeId => {
                            if node_id.is_some() {
                                return Err(serde::de::Error::duplicate_field("node_id"));
                            }
                            node_id = Some(map.next_value()?);
                        }
                    }
                }

                let actor_id = actor_id.ok_or_else(|| serde::de::Error::missing_field("actor_id"))?;
                let node_id = node_id.ok_or_else(|| serde::de::Error::missing_field("node_id"))?;

                Ok(ActorAddress {
                    actor_id,
                    node_id,
                    activation_time: Instant::now(), // Set to current time on deserialize
                })
            }
        }

        const FIELDS: &[&str] = &["actor_id", "node_id"];
        deserializer.deserialize_struct("ActorAddress", FIELDS, ActorAddressVisitor)
    }
}

// Manual PartialEq that ignores activation_time for comparison
impl PartialEq for ActorAddress {
    fn eq(&self, other: &Self) -> bool {
        self.actor_id == other.actor_id && self.node_id == other.node_id
    }
}

impl Eq for ActorAddress {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direction_enum() {
        assert_eq!(Direction::Request, Direction::Request);
        assert_ne!(Direction::Request, Direction::Response);
    }

    #[test]
    fn test_message_flags() {
        let flags = MessageFlags::READ_ONLY | MessageFlags::ALWAYS_INTERLEAVE;
        assert!(flags.contains(MessageFlags::READ_ONLY));
        assert!(flags.contains(MessageFlags::ALWAYS_INTERLEAVE));
        assert!(!flags.contains(MessageFlags::IS_LOCAL_ONLY));
    }

    #[test]
    fn test_message_request_creation() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let msg = Message::request(
            CorrelationId::new(1),
            target,
            sender,
            target_node,
            sender_node,
            "DepositRequest".to_string(),
            vec![1, 2, 3],
            std::time::Duration::from_secs(30),
        );

        assert_eq!(msg.direction, Direction::Request);
        assert_eq!(msg.correlation_id, CorrelationId::new(1));
        assert!(msg.time_to_expiry.is_some());
        assert_eq!(msg.forward_count, 0);
    }

    #[test]
    fn test_message_response_creation() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let request = Message::request(
            CorrelationId::new(1),
            target.clone(),
            sender.clone(),
            target_node.clone(),
            sender_node.clone(),
            "DepositRequest".to_string(),
            vec![1, 2, 3],
            std::time::Duration::from_secs(30),
        );

        let response = Message::response(&request, vec![4, 5, 6]);

        assert_eq!(response.direction, Direction::Response);
        assert_eq!(response.correlation_id, CorrelationId::new(1));
        // Target/sender swapped
        assert_eq!(response.target_actor, sender);
        assert_eq!(response.sender_actor, target);
        assert_eq!(response.target_node, sender_node);
        assert_eq!(response.sender_node, target_node);
    }

    #[test]
    fn test_message_oneway_creation() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let msg = Message::oneway(
            target,
            sender,
            target_node,
            sender_node,
            "Notification".to_string(),
            vec![1, 2, 3],
        );

        assert_eq!(msg.direction, Direction::OneWay);
        assert!(msg.time_to_expiry.is_none());
    }

    #[test]
    fn test_message_forward_count() {
        let target = ActorId::new("prod", "BankAccount", "alice");
        let sender = ActorId::new("prod", "Client", "system");
        let target_node = NodeId::from("127.0.0.1:5000").unwrap();
        let sender_node = NodeId::from("127.0.0.1:5001").unwrap();

        let mut msg = Message::request(
            CorrelationId::new(1),
            target,
            sender,
            target_node,
            sender_node,
            "DepositRequest".to_string(),
            vec![],
            std::time::Duration::from_secs(30),
        );

        assert_eq!(msg.forward_count, 0);

        // First forward succeeds
        assert!(msg.increment_forward_count(2).is_ok());
        assert_eq!(msg.forward_count, 1);

        // Second forward succeeds
        assert!(msg.increment_forward_count(2).is_ok());
        assert_eq!(msg.forward_count, 2);

        // Third forward fails (exceeds max)
        assert!(msg.increment_forward_count(2).is_err());
    }
}
