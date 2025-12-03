//! Error types for request-response operations.
//!
//! These errors can occur during the lifecycle of a request:
//! - Server drops the promise without responding ([`ReplyError::BrokenPromise`])
//! - Network connection fails ([`ReplyError::ConnectionFailed`])
//! - Request times out ([`ReplyError::Timeout`])
//! - Serialization/deserialization fails ([`ReplyError::Serialization`])

use serde::{Deserialize, Serialize};

/// Errors that can occur during request-response operations.
///
/// These errors are serializable so they can be sent over the network
/// (e.g., when a server sends a BrokenPromise error to the client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyError {
    /// The server dropped the ReplyPromise without sending a response.
    ///
    /// This typically indicates a bug in the server code - forgetting to
    /// reply before the promise goes out of scope.
    BrokenPromise,

    /// The network connection failed during the request.
    ///
    /// The request may or may not have been delivered to the server.
    ConnectionFailed,

    /// The request timed out waiting for a response.
    ///
    /// The server may still be processing the request.
    Timeout,

    /// Serialization or deserialization failed.
    ///
    /// Contains a description of what went wrong.
    Serialization {
        /// Human-readable error message.
        message: String,
    },

    /// The endpoint was not found.
    ///
    /// The destination endpoint is not registered.
    EndpointNotFound,
}

impl std::fmt::Display for ReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplyError::BrokenPromise => write!(f, "server dropped promise without reply"),
            ReplyError::ConnectionFailed => write!(f, "connection failed"),
            ReplyError::Timeout => write!(f, "request timed out"),
            ReplyError::Serialization { message } => write!(f, "serialization error: {}", message),
            ReplyError::EndpointNotFound => write!(f, "endpoint not found"),
        }
    }
}

impl std::error::Error for ReplyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reply_error_display() {
        assert_eq!(
            ReplyError::BrokenPromise.to_string(),
            "server dropped promise without reply"
        );
        assert_eq!(
            ReplyError::ConnectionFailed.to_string(),
            "connection failed"
        );
        assert_eq!(ReplyError::Timeout.to_string(), "request timed out");
        assert_eq!(
            ReplyError::Serialization {
                message: "invalid JSON".to_string()
            }
            .to_string(),
            "serialization error: invalid JSON"
        );
        assert_eq!(
            ReplyError::EndpointNotFound.to_string(),
            "endpoint not found"
        );
    }

    #[test]
    fn test_reply_error_serde_roundtrip() {
        let errors = vec![
            ReplyError::BrokenPromise,
            ReplyError::ConnectionFailed,
            ReplyError::Timeout,
            ReplyError::Serialization {
                message: "test error".to_string(),
            },
            ReplyError::EndpointNotFound,
        ];

        for error in errors {
            let json = serde_json::to_string(&error).expect("serialize");
            let decoded: ReplyError = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(error, decoded);
        }
    }

    #[test]
    fn test_reply_error_is_error_trait() {
        let error: Box<dyn std::error::Error> = Box::new(ReplyError::BrokenPromise);
        assert!(error.to_string().contains("promise"));
    }
}
