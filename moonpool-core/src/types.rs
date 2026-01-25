//! Core types for endpoint addressing.
//!
//! This module provides the fundamental types for network addressing in moonpool:
//! - [`UID`]: 128-bit unique identifier for endpoints
//! - [`NetworkAddress`]: IP address + port + flags
//! - [`Endpoint`]: Complete destination = address + token

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::well_known::WellKnownToken;

/// 128-bit unique identifier.
///
/// Well-known UIDs use `first = u64::MAX`. Random UIDs have both parts
/// randomly generated.
///
/// # Examples
///
/// ```
/// use moonpool_core::UID;
///
/// // Create a well-known UID for system services
/// let ping_token = UID::well_known(1);
/// assert!(ping_token.is_well_known());
///
/// // Create a regular UID
/// let uid = UID::new(0x123, 0x456);
/// assert!(!uid.is_well_known());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct UID {
    /// First 64 bits. For well-known UIDs, this is `u64::MAX`.
    pub first: u64,
    /// Second 64 bits. For well-known UIDs, this is the token ID.
    pub second: u64,
}

impl UID {
    /// Create a new UID with explicit values.
    pub const fn new(first: u64, second: u64) -> Self {
        Self { first, second }
    }

    /// Create a well-known UID (FDB pattern: first = -1).
    ///
    /// Well-known UIDs are used for system services that need deterministic
    /// addressing without discovery (e.g., Ping, EndpointNotFound).
    pub const fn well_known(token_id: u32) -> Self {
        Self {
            first: u64::MAX,
            second: token_id as u64,
        }
    }

    /// Check if this is a well-known UID.
    ///
    /// Well-known UIDs have `first == u64::MAX`, allowing O(1) lookup
    /// in the endpoint map via array indexing.
    pub const fn is_well_known(&self) -> bool {
        self.first == u64::MAX
    }

    /// Create adjusted UID for interface serialization (FDB pattern).
    ///
    /// Used to derive multiple endpoints from a single base token.
    /// This allows serializing only one endpoint for an interface,
    /// with others derived using this method.
    ///
    /// # FDB Implementation
    /// From FlowTransport.h:
    /// ```cpp
    /// UID(token.first() + (uint64_t(index) << 32),
    ///     (token.second() & 0xffffffff00000000LL) | newIndex)
    /// ```
    pub const fn adjusted(&self, index: u32) -> Self {
        Self {
            first: self.first.wrapping_add((index as u64) << 32),
            second: (self.second & 0xffffffff00000000)
                | ((self.second as u32).wrapping_add(index) as u64),
        }
    }

    /// Check if UID is valid (non-zero).
    pub const fn is_valid(&self) -> bool {
        self.first != 0 || self.second != 0
    }
}

impl std::fmt::Display for UID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}{:016x}", self.first, self.second)
    }
}

/// Address flags.
pub mod flags {
    /// Connection uses TLS encryption.
    pub const FLAG_TLS: u16 = 1;
    /// Address is publicly routable (vs internal).
    pub const FLAG_PUBLIC: u16 = 2;
}

/// Network address (IPv4/IPv6 + port + flags).
///
/// # Examples
///
/// ```
/// use moonpool_core::NetworkAddress;
/// use std::net::{IpAddr, Ipv4Addr};
///
/// let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
/// assert_eq!(addr.to_string(), "127.0.0.1:4500");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkAddress {
    /// IP address (IPv4 or IPv6).
    pub ip: IpAddr,
    /// Port number.
    pub port: u16,
    /// Address flags (TLS, public routing).
    pub flags: u16,
}

impl NetworkAddress {
    /// Create a new network address without flags.
    pub fn new(ip: IpAddr, port: u16) -> Self {
        Self { ip, port, flags: 0 }
    }

    /// Create a new network address with flags.
    pub fn with_flags(ip: IpAddr, port: u16, flags: u16) -> Self {
        Self { ip, port, flags }
    }

    /// Check if this address uses TLS.
    pub fn is_tls(&self) -> bool {
        self.flags & flags::FLAG_TLS != 0
    }

    /// Check if this address is publicly routable.
    pub fn is_public(&self) -> bool {
        self.flags & flags::FLAG_PUBLIC != 0
    }

    /// Parse from string "ip:port" format.
    ///
    /// Supports both IPv4 (`127.0.0.1:4500`) and IPv6 (`[::1]:4500`) notation.
    ///
    /// # Errors
    ///
    /// Returns error if IP or port cannot be parsed.
    pub fn parse(s: &str) -> Result<Self, NetworkAddressParseError> {
        // Handle IPv6 bracket notation [::1]:port
        if let Some(bracket_end) = s.rfind(']') {
            if !s.starts_with('[') {
                return Err(NetworkAddressParseError::InvalidIp);
            }
            let ip_str = &s[1..bracket_end];
            let port_str = s
                .get(bracket_end + 2..)
                .ok_or(NetworkAddressParseError::MissingPort)?;
            let ip: IpAddr = ip_str
                .parse()
                .map_err(|_| NetworkAddressParseError::InvalidIp)?;
            let port: u16 = port_str
                .parse()
                .map_err(|_| NetworkAddressParseError::InvalidPort)?;
            Ok(Self::new(ip, port))
        } else {
            // IPv4 format ip:port
            let (ip_str, port_str) = s
                .rsplit_once(':')
                .ok_or(NetworkAddressParseError::MissingPort)?;
            let ip: IpAddr = ip_str
                .parse()
                .map_err(|_| NetworkAddressParseError::InvalidIp)?;
            let port: u16 = port_str
                .parse()
                .map_err(|_| NetworkAddressParseError::InvalidPort)?;
            Ok(Self::new(ip, port))
        }
    }
}

impl std::fmt::Display for NetworkAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.ip {
            IpAddr::V4(ip) => write!(f, "{}:{}", ip, self.port),
            IpAddr::V6(ip) => write!(f, "[{}]:{}", ip, self.port),
        }
    }
}

/// Error parsing a network address from string.
#[derive(Debug, Clone, thiserror::Error)]
pub enum NetworkAddressParseError {
    /// The IP address could not be parsed.
    #[error("invalid IP address")]
    InvalidIp,
    /// The port number could not be parsed.
    #[error("invalid port number")]
    InvalidPort,
    /// No port separator (`:`) found in the input.
    #[error("missing port separator")]
    MissingPort,
}

/// Endpoint = Address + Token.
///
/// Represents a unique destination for messages. The combination of
/// network address and UID token allows direct addressing without
/// service discovery.
///
/// # Examples
///
/// ```
/// use moonpool_core::{Endpoint, NetworkAddress, UID, WellKnownToken};
/// use std::net::{IpAddr, Ipv4Addr};
///
/// let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
/// let endpoint = Endpoint::well_known(addr, WellKnownToken::Ping);
///
/// assert!(endpoint.token.is_well_known());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Network address for this endpoint.
    pub address: NetworkAddress,
    /// Unique token identifying the endpoint.
    pub token: UID,
}

impl Endpoint {
    /// Create a new endpoint.
    pub fn new(address: NetworkAddress, token: UID) -> Self {
        Self { address, token }
    }

    /// Create a well-known endpoint.
    pub fn well_known(address: NetworkAddress, token: WellKnownToken) -> Self {
        Self {
            address,
            token: token.uid(),
        }
    }

    /// Get adjusted endpoint for interface serialization (FDB pattern).
    ///
    /// Creates a new endpoint with the same address but an adjusted token.
    pub fn adjusted(&self, index: u32) -> Self {
        Self {
            address: self.address.clone(),
            token: self.token.adjusted(index),
        }
    }

    /// Check if endpoint is valid (has valid token).
    pub fn is_valid(&self) -> bool {
        self.token.is_valid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_uid_well_known() {
        let uid = UID::well_known(42);
        assert!(uid.is_well_known());
        assert_eq!(uid.first, u64::MAX);
        assert_eq!(uid.second, 42);
    }

    #[test]
    fn test_uid_regular() {
        let uid = UID::new(123, 456);
        assert!(!uid.is_well_known());
        assert!(uid.is_valid());
    }

    #[test]
    fn test_uid_default_invalid() {
        let uid = UID::default();
        assert!(!uid.is_valid());
    }

    #[test]
    fn test_uid_adjusted() {
        let base = UID::new(0x1000, 0x2000);
        let adj1 = base.adjusted(1);
        let adj2 = base.adjusted(2);

        // Each adjusted UID should be unique
        assert_ne!(base, adj1);
        assert_ne!(adj1, adj2);
        assert_ne!(base, adj2);

        // Adjusting with 0 should give same result as original
        let adj0 = base.adjusted(0);
        assert_eq!(base, adj0);
    }

    #[test]
    fn test_uid_display() {
        let uid = UID::new(0x123456789ABCDEF0, 0xFEDCBA9876543210);
        assert_eq!(uid.to_string(), "123456789abcdef0fedcba9876543210");
    }

    #[test]
    fn test_network_address_ipv4() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 4500);
        assert_eq!(addr.to_string(), "192.168.1.1:4500");
        assert!(!addr.is_tls());
        assert!(!addr.is_public());
    }

    #[test]
    fn test_network_address_ipv6() {
        let addr = NetworkAddress::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 4500);
        assert_eq!(addr.to_string(), "[::1]:4500");
    }

    #[test]
    fn test_network_address_flags() {
        let addr = NetworkAddress::with_flags(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            4500,
            flags::FLAG_TLS | flags::FLAG_PUBLIC,
        );
        assert!(addr.is_tls());
        assert!(addr.is_public());
    }

    #[test]
    fn test_network_address_parse_ipv4() {
        let addr = NetworkAddress::parse("127.0.0.1:4500").expect("parse");
        assert_eq!(addr.ip, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(addr.port, 4500);
    }

    #[test]
    fn test_network_address_parse_ipv6() {
        let addr = NetworkAddress::parse("[::1]:4500").expect("parse");
        assert_eq!(addr.ip, IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(addr.port, 4500);
    }

    #[test]
    fn test_network_address_parse_errors() {
        assert!(NetworkAddress::parse("invalid").is_err());
        assert!(NetworkAddress::parse("127.0.0.1").is_err()); // missing port
        assert!(NetworkAddress::parse("127.0.0.1:abc").is_err()); // invalid port
        assert!(NetworkAddress::parse("not_an_ip:4500").is_err()); // invalid IP
    }

    #[test]
    fn test_endpoint_well_known() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let endpoint = Endpoint::well_known(addr, WellKnownToken::Ping);

        assert!(endpoint.token.is_well_known());
        assert!(endpoint.is_valid());
    }

    #[test]
    fn test_endpoint_adjusted() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let base = Endpoint::new(addr, UID::new(100, 200));

        let adj1 = base.adjusted(1);
        let adj2 = base.adjusted(2);

        // Addresses should be the same
        assert_eq!(base.address, adj1.address);
        assert_eq!(base.address, adj2.address);

        // Tokens should be different
        assert_ne!(base.token, adj1.token);
        assert_ne!(adj1.token, adj2.token);
    }

    #[test]
    fn test_uid_serde_roundtrip() {
        let uid = UID::new(0x123456789ABCDEF0, 0xFEDCBA9876543210);
        let json = serde_json::to_string(&uid).expect("serialize");
        let decoded: UID = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(uid, decoded);
    }

    #[test]
    fn test_uid_well_known_serde() {
        let uid = UID::well_known(42);
        let json = serde_json::to_string(&uid).expect("serialize");
        let decoded: UID = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(uid, decoded);
        assert!(decoded.is_well_known());
    }

    #[test]
    fn test_network_address_serde_ipv4() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 4500);
        let json = serde_json::to_string(&addr).expect("serialize");
        let decoded: NetworkAddress = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(addr, decoded);
    }

    #[test]
    fn test_network_address_serde_ipv6() {
        let addr = NetworkAddress::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 4500);
        let json = serde_json::to_string(&addr).expect("serialize");
        let decoded: NetworkAddress = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(addr, decoded);
    }

    #[test]
    fn test_network_address_serde_with_flags() {
        let addr = NetworkAddress::with_flags(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            4500,
            flags::FLAG_TLS | flags::FLAG_PUBLIC,
        );
        let json = serde_json::to_string(&addr).expect("serialize");
        let decoded: NetworkAddress = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(addr, decoded);
        assert!(decoded.is_tls());
        assert!(decoded.is_public());
    }

    #[test]
    fn test_endpoint_serde_roundtrip() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let endpoint = Endpoint::new(addr, UID::new(100, 200));
        let json = serde_json::to_string(&endpoint).expect("serialize");
        let decoded: Endpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(endpoint, decoded);
    }

    #[test]
    fn test_endpoint_well_known_serde() {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        let endpoint = Endpoint::well_known(addr, WellKnownToken::Ping);
        let json = serde_json::to_string(&endpoint).expect("serialize");
        let decoded: Endpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(endpoint, decoded);
        assert!(decoded.token.is_well_known());
    }
}
