//! Macros for reducing RPC boilerplate.
//!
//! This module provides macros to simplify common patterns in RPC development:
//!
//! - [`rpc_messages!`]: Auto-derive Serialize, Deserialize, Debug, Clone for message types
//! - [`rpc_interface!`]: Define multi-method RPC interfaces with auto-generated tokens
//!
//! # Example
//!
//! ```rust
//! use moonpool_transport::{rpc_messages, rpc_interface};
//!
//! // Define request/response pairs with required derives
//! rpc_messages! {
//!     /// Request to add two numbers
//!     pub struct AddRequest {
//!         pub a: i64,
//!         pub b: i64,
//!     }
//!
//!     /// Response with the sum
//!     pub struct AddResponse {
//!         pub result: i64,
//!     }
//! }
//!
//! // Define an interface with multiple methods
//! rpc_interface! {
//!     /// Calculator service interface
//!     pub Calculator(0xCA1C_0000) {
//!         /// Add two numbers
//!         add: 0,
//!         /// Subtract two numbers
//!         sub: 1,
//!         /// Multiply two numbers
//!         mul: 2,
//!     }
//! }
//! ```

/// Define RPC message types with required derives.
///
/// This macro automatically adds `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]`
/// to each struct, reducing boilerplate for message definitions.
///
/// # Example
///
/// ```rust
/// use moonpool_transport::rpc_messages;
///
/// rpc_messages! {
///     /// A ping request
///     pub struct PingRequest {
///         pub seq: u32,
///     }
///
///     /// A ping response
///     pub struct PingResponse {
///         pub seq: u32,
///         pub timestamp: u64,
///     }
/// }
/// ```
///
/// This expands to:
///
/// ```rust
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// pub struct PingRequest {
///     pub seq: u32,
/// }
///
/// #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// pub struct PingResponse {
///     pub seq: u32,
///     pub timestamp: u64,
/// }
/// ```
#[macro_export]
macro_rules! rpc_messages {
    (
        $(
            $(#[$meta:meta])*
            $vis:vis struct $name:ident {
                $(
                    $(#[$field_meta:meta])*
                    $field_vis:vis $field:ident : $ty:ty
                ),* $(,)?
            }
        )*
    ) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
            $vis struct $name {
                $(
                    $(#[$field_meta])*
                    $field_vis $field : $ty,
                )*
            }
        )*
    };
}

/// Define an RPC interface with multiple methods.
///
/// This macro generates:
/// - A struct with the interface name
/// - Constants for each method's UID token
/// - A helper method to create endpoints for all methods
///
/// # Example
///
/// ```rust
/// use moonpool_transport::rpc_interface;
///
/// rpc_interface! {
///     /// Calculator service with basic arithmetic operations
///     pub Calculator(0xCA1C_0000) {
///         /// Addition operation
///         add: 0,
///         /// Subtraction operation
///         sub: 1,
///         /// Multiplication operation
///         mul: 2,
///         /// Division operation
///         div: 3,
///     }
/// }
///
/// // Usage:
/// use moonpool_transport::{Endpoint, NetworkAddress, UID};
///
/// // Access method tokens directly
/// let add_token: UID = Calculator::ADD;
/// let sub_token: UID = Calculator::SUB;
///
/// // Create endpoints for a server address
/// let server_addr = NetworkAddress::parse("127.0.0.1:4500").unwrap();
/// let add_endpoint = Endpoint::new(server_addr.clone(), Calculator::ADD);
/// ```
#[macro_export]
macro_rules! rpc_interface {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident ($base:expr) {
            $(
                $(#[$method_meta:meta])*
                $method:ident : $index:expr
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name;

        impl $name {
            /// Base token for this interface.
            pub const BASE: u64 = $base;

            $(
                $(#[$method_meta])*
                #[allow(non_upper_case_globals)]
                pub const $method: $crate::UID = $crate::UID::new($base, $index);
            )*

            /// Create an endpoint for a specific method.
            ///
            /// # Arguments
            ///
            /// * `server` - The server's network address
            /// * `token` - The method's UID token (e.g., `Calculator::add`)
            #[inline]
            pub fn endpoint(server: &$crate::NetworkAddress, token: $crate::UID) -> $crate::Endpoint {
                $crate::Endpoint::new(server.clone(), token)
            }
        }
    };
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use crate::{NetworkAddress, UID};

    rpc_messages! {
        /// Test request
        pub struct TestRequest {
            pub id: u32,
            pub data: String,
        }

        /// Test response
        pub struct TestResponse {
            pub success: bool,
        }
    }

    rpc_interface! {
        /// Test interface
        pub TestService(0x1234_0000) {
            /// Method one
            method_one: 0,
            /// Method two
            method_two: 1,
        }
    }

    #[test]
    fn test_rpc_messages_macro() {
        let req = TestRequest {
            id: 42,
            data: "hello".to_string(),
        };
        assert_eq!(req.id, 42);
        assert_eq!(req.data, "hello");

        // Test serialization works
        let json = serde_json::to_string(&req).expect("serialize");
        let parsed: TestRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.id, 42);
    }

    #[test]
    fn test_rpc_interface_macro() {
        // Test that tokens are generated correctly
        assert_eq!(TestService::BASE, 0x1234_0000);
        assert_eq!(TestService::method_one, UID::new(0x1234_0000, 0));
        assert_eq!(TestService::method_two, UID::new(0x1234_0000, 1));

        // Test endpoint creation
        let addr = NetworkAddress::parse("127.0.0.1:4500").expect("parse");
        let endpoint = TestService::endpoint(&addr, TestService::method_one);
        assert_eq!(endpoint.token, TestService::method_one);
    }
}
