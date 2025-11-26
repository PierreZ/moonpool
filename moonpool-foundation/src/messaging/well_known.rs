//! Well-known endpoint tokens for system services.
//!
//! These are compile-time constants matching FDB's WLTOKEN_* enum.
//! Well-known tokens enable O(1) lookup in the endpoint map via array indexing
//! (first part is u64::MAX, second part is the index).
//!
//! Note: The actual receivers for these tokens are implemented in the moonpool
//! crate, not in foundation. Foundation only provides the constants.

use super::types::UID;

/// Well-known endpoint tokens (FDB-compatible pattern).
///
/// These tokens are used for system services that need deterministic addressing
/// without service discovery. Clients can construct endpoints directly using
/// these well-known tokens.
///
/// # FDB Reference
/// From FlowTransport.h:
/// ```cpp
/// enum { WLTOKEN_ENDPOINT_NOT_FOUND = 0, WLTOKEN_PING_PACKET,
///        WLTOKEN_UNAUTHORIZED_ENDPOINT, WLTOKEN_FIRST_AVAILABLE };
/// ```
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WellKnownToken {
    /// Response sent when an endpoint is not found.
    /// Used to notify senders that their target doesn't exist.
    EndpointNotFound = 0,

    /// Ping for connection health checks.
    /// Used by the connection monitor to detect failures.
    Ping = 1,

    /// Unauthorized endpoint access response.
    /// Sent when a client tries to access a protected endpoint.
    UnauthorizedEndpoint = 2,

    /// First available token for user services.
    /// User-defined well-known services should use this value or higher.
    FirstAvailable = 3,
}

impl WellKnownToken {
    /// Convert to u32 for UID creation.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Create a UID for this well-known token.
    pub const fn uid(self) -> UID {
        UID::well_known(self as u32)
    }
}

/// Number of reserved well-known token slots.
///
/// The endpoint map reserves an array of this size for O(1) lookup
/// of well-known endpoints. Tokens 0-63 are reserved for system use.
///
/// Matches FDB's WLTOKEN_RESERVED_COUNT pattern.
pub const WELL_KNOWN_RESERVED_COUNT: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_well_known_token_values() {
        assert_eq!(WellKnownToken::EndpointNotFound.as_u32(), 0);
        assert_eq!(WellKnownToken::Ping.as_u32(), 1);
        assert_eq!(WellKnownToken::UnauthorizedEndpoint.as_u32(), 2);
        assert_eq!(WellKnownToken::FirstAvailable.as_u32(), 3);
    }

    #[test]
    fn test_well_known_uid() {
        let uid = WellKnownToken::Ping.uid();
        assert!(uid.is_well_known());
        assert_eq!(uid.first, u64::MAX);
        assert_eq!(uid.second, 1);
    }

    #[test]
    fn test_reserved_count() {
        // Ensure all defined tokens fit within reserved count
        assert!((WellKnownToken::FirstAvailable.as_u32() as usize) < WELL_KNOWN_RESERVED_COUNT);
    }
}
