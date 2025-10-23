//! Core identifier types for actors and nodes.

use crate::error::{ActorIdError, NodeIdError};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddr;

/// Unique identifier for a virtual actor, independent of physical location.
///
/// # Structure
///
/// - `namespace`: Logical namespace for isolation (e.g., "prod", "staging", "tenant-123")
/// - `actor_type`: Type of actor (e.g., "BankAccount", "User", "Session")
/// - `key`: Unique key within the namespace+type (e.g., "alice", "account-456")
///
/// # String Format
///
/// `namespace::actor_type/key`
///
/// Examples: `prod::BankAccount/alice`, `tenant-acme::User/bob`
///
/// # Validation Rules
///
/// - All fields must be non-empty
/// - Total length should not exceed 256 characters
/// - Fields should contain only alphanumeric, dash, underscore for safety
///
/// # Namespace Assignment
///
/// Namespace is set at ActorRuntime bootstrap (cluster-wide configuration).
/// All actors within a cluster share the same namespace. Cannot send messages
/// across namespaces (namespace boundary = cluster boundary).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorId {
    pub namespace: String,
    pub actor_type: String,
    pub key: String,
}

impl ActorId {
    /// Create a new ActorId (internal use by ActorRuntime).
    ///
    /// # Panics
    ///
    /// Panics if any field is empty.
    #[allow(dead_code)]
    pub(crate) fn new(
        namespace: impl Into<String>,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        let namespace = namespace.into();
        let actor_type = actor_type.into();
        let key = key.into();

        assert!(!namespace.is_empty(), "namespace cannot be empty");
        assert!(!actor_type.is_empty(), "actor_type cannot be empty");
        assert!(!key.is_empty(), "key cannot be empty");

        Self {
            namespace,
            actor_type,
            key,
        }
    }

    /// Create ActorId from individual parts (public API for ActorRuntime).
    ///
    /// # Errors
    ///
    /// Returns error if any field is empty.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let actor_id = ActorId::from_parts("prod", "BankAccount", "alice")?;
    /// ```
    pub fn from_parts(
        namespace: impl Into<String>,
        actor_type: impl Into<String>,
        key: impl Into<String>,
    ) -> Result<Self, ActorIdError> {
        let namespace = namespace.into();
        let actor_type = actor_type.into();
        let key = key.into();

        if namespace.is_empty() {
            return Err(ActorIdError::EmptyField("namespace".to_string()));
        }
        if actor_type.is_empty() {
            return Err(ActorIdError::EmptyField("actor_type".to_string()));
        }
        if key.is_empty() {
            return Err(ActorIdError::EmptyField("key".to_string()));
        }

        Ok(Self {
            namespace,
            actor_type,
            key,
        })
    }

    /// Parse ActorId from string format: `namespace::actor_type/key`
    ///
    /// # Errors
    ///
    /// Returns `ActorIdError::InvalidFormat` if format is invalid.
    pub fn from_string(s: &str) -> Result<Self, ActorIdError> {
        let parts: Vec<&str> = s.split("::").collect();
        if parts.len() != 2 {
            return Err(ActorIdError::InvalidFormat);
        }

        let namespace = parts[0];
        if namespace.is_empty() {
            return Err(ActorIdError::EmptyField("namespace".to_string()));
        }

        let actor_and_key: Vec<&str> = parts[1].split('/').collect();
        if actor_and_key.len() != 2 {
            return Err(ActorIdError::InvalidFormat);
        }

        let actor_type = actor_and_key[0];
        let key = actor_and_key[1];

        if actor_type.is_empty() {
            return Err(ActorIdError::EmptyField("actor_type".to_string()));
        }
        if key.is_empty() {
            return Err(ActorIdError::EmptyField("key".to_string()));
        }

        Ok(Self {
            namespace: namespace.to_string(),
            actor_type: actor_type.to_string(),
            key: key.to_string(),
        })
    }

    /// Convert to string format: `namespace::actor_type/key`
    pub fn to_string_format(&self) -> String {
        format!("{}::{}/{}", self.namespace, self.actor_type, self.key)
    }

    /// Get the actor type.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let actor_id = ActorId::from_parts("prod", "BankAccount", "alice")?;
    /// assert_eq!(actor_id.actor_type(), "BankAccount");
    /// ```
    pub fn actor_type(&self) -> &str {
        &self.actor_type
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}/{}", self.namespace, self.actor_type, self.key)
    }
}

/// Identifies a physical node in the cluster by its network address.
///
/// # Format
///
/// `host:port` where host can be IPv4, IPv6, or hostname.
///
/// # Examples
///
/// - `127.0.0.1:5000`
/// - `192.168.1.100:8080`
/// - `node1.cluster.local:5000`
///
/// # Validation Rules
///
/// - Must be in "host:port" format with valid separator ':'
/// - Port must be valid u16 (0-65535)
/// - Must be unique within the cluster
/// - Must remain stable during node lifetime
/// - Should be routable/reachable from all cluster nodes
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(String);

impl NodeId {
    /// Create NodeId from address:port string.
    ///
    /// # Errors
    ///
    /// Returns `NodeIdError::InvalidFormat` if format is invalid.
    pub fn from(addr: impl Into<String>) -> Result<Self, NodeIdError> {
        let addr = addr.into();
        Self::validate(&addr)?;
        Ok(Self(addr))
    }

    /// Parse from SocketAddr.
    pub fn from_socket_addr(addr: SocketAddr) -> Self {
        Self(addr.to_string())
    }

    /// Validate address format.
    fn validate(addr: &str) -> Result<(), NodeIdError> {
        if !addr.contains(':') {
            return Err(NodeIdError::InvalidFormat);
        }

        // Try to parse as SocketAddr for validation
        if addr.parse::<SocketAddr>().is_err() {
            return Err(NodeIdError::InvalidAddress);
        }

        Ok(())
    }

    /// Get host portion (before colon).
    pub fn host(&self) -> &str {
        self.0.split(':').next().unwrap_or(&self.0)
    }

    /// Get port portion (after colon).
    pub fn port(&self) -> Option<u16> {
        self.0.split(':').nth(1)?.parse().ok()
    }

    /// Convert to SocketAddr for binding/connecting.
    pub fn to_socket_addr(&self) -> Result<SocketAddr, NodeIdError> {
        self.0.parse().map_err(|_| NodeIdError::InvalidAddress)
    }

    /// Get the raw address string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for matching requests to responses in request-response messaging.
///
/// # Structure
///
/// A monotonically increasing u64 counter per node.
///
/// # Validation Rules
///
/// - Must be unique per sender node (not globally unique)
/// - Must not be reused within reasonable time window
///
/// # Invariants
///
/// - Response CorrelationId matches request CorrelationId
/// - Single CorrelationId corresponds to exactly one pending request
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CorrelationId(pub u64);

impl CorrelationId {
    /// Create a new CorrelationId.
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    /// Get the raw ID value.
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_actor_id_creation() {
        let id = ActorId::new("prod", "BankAccount", "alice");
        assert_eq!(id.namespace, "prod");
        assert_eq!(id.actor_type, "BankAccount");
        assert_eq!(id.key, "alice");
    }

    #[test]
    fn test_actor_id_string_format() {
        let id = ActorId::new("prod", "BankAccount", "alice");
        assert_eq!(id.to_string_format(), "prod::BankAccount/alice");
        assert_eq!(id.to_string(), "prod::BankAccount/alice");
    }

    #[test]
    fn test_actor_id_from_string() {
        let id = ActorId::from_string("prod::BankAccount/alice").unwrap();
        assert_eq!(id.namespace, "prod");
        assert_eq!(id.actor_type, "BankAccount");
        assert_eq!(id.key, "alice");
    }

    #[test]
    fn test_actor_id_from_string_invalid_format() {
        assert!(ActorId::from_string("invalid").is_err());
        assert!(ActorId::from_string("prod::alice").is_err()); // missing key
        assert!(ActorId::from_string("prod/alice").is_err()); // wrong separator
        assert!(ActorId::from_string("::BankAccount/alice").is_err()); // empty namespace
        assert!(ActorId::from_string("prod::/alice").is_err()); // empty actor_type
        assert!(ActorId::from_string("prod::BankAccount/").is_err()); // empty key
    }

    #[test]
    fn test_node_id_creation() {
        let node = NodeId::from("127.0.0.1:5000").unwrap();
        assert_eq!(node.as_str(), "127.0.0.1:5000");
        assert_eq!(node.host(), "127.0.0.1");
        assert_eq!(node.port(), Some(5000));
    }

    #[test]
    fn test_node_id_from_socket_addr() {
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let node = NodeId::from_socket_addr(addr);
        assert_eq!(node.to_socket_addr().unwrap(), addr);
    }

    #[test]
    fn test_node_id_invalid_format() {
        assert!(NodeId::from("invalid").is_err());
        assert!(NodeId::from("127.0.0.1").is_err()); // missing port
        assert!(NodeId::from("127.0.0.1:99999").is_err()); // invalid port
    }

    #[test]
    fn test_correlation_id() {
        let id = CorrelationId::new(42);
        assert_eq!(id.as_u64(), 42);
        assert_eq!(id.to_string(), "42");
    }
}
