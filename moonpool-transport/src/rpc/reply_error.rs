//! Error types for request-response operations.
//!
//! These errors can occur during the lifecycle of a request:
//! - Server drops the promise without responding ([`ReplyError::BrokenPromise`])
//! - Server handler returns an error ([`ReplyError::Application`])
//! - Network connection fails ([`ReplyError::ConnectionFailed`])
//! - Request times out ([`ReplyError::Timeout`])
//! - Serialization/deserialization fails ([`ReplyError::Serialization`])

use serde::{Deserialize, Serialize};

/// Errors that can occur during request-response operations.
///
/// These errors are serializable so they can be sent over the network
/// (e.g., when a server sends a `BrokenPromise` error to the client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[non_exhaustive]
pub enum ReplyError {
    /// The server dropped the `ReplyPromise` without sending a response.
    ///
    /// This signals server liveness loss: the actor that owned the promise
    /// went away (panic, drop, runtime teardown) without calling `send` or
    /// `send_error`. The client receiving this maps the endpoint to
    /// permanently failed because the `WaitFailure` pattern relies on it.
    #[error("server dropped promise without reply")]
    BrokenPromise,

    /// The handler returned an error.
    ///
    /// Distinct from [`BrokenPromise`](Self::BrokenPromise): the server is
    /// alive and chose to fail this request. Future requests to the same
    /// endpoint should not be short-circuited.
    #[error("handler error: {message}")]
    Application {
        /// Stringified handler error.
        message: String,
    },

    /// The network connection failed during the request.
    ///
    /// The request may or may not have been delivered to the server.
    #[error("connection failed")]
    ConnectionFailed,

    /// The request timed out waiting for a response.
    ///
    /// The server may still be processing the request.
    #[error("request timed out")]
    Timeout,

    /// Serialization or deserialization failed.
    ///
    /// Contains a description of what went wrong.
    #[error("serialization error: {message}")]
    Serialization {
        /// Human-readable error message.
        message: String,
    },

    /// The endpoint was not found.
    ///
    /// The destination endpoint is not registered.
    #[error("endpoint not found")]
    EndpointNotFound,

    /// The peer disconnected while the request was in flight.
    ///
    /// The request may or may not have been delivered and processed.
    /// Callers must handle this ambiguity (e.g., query state before retrying).
    ///
    /// FDB: `request_maybe_delivered` (error 1030)
    #[error("peer disconnected, delivery uncertain")]
    MaybeDelivered,
}

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
            ReplyError::Application {
                message: "user not found".to_string()
            }
            .to_string(),
            "handler error: user not found"
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
        assert_eq!(
            ReplyError::MaybeDelivered.to_string(),
            "peer disconnected, delivery uncertain"
        );
    }

    #[test]
    fn test_reply_error_serde_roundtrip() {
        let errors = vec![
            ReplyError::BrokenPromise,
            ReplyError::Application {
                message: "boom".to_string(),
            },
            ReplyError::ConnectionFailed,
            ReplyError::Timeout,
            ReplyError::Serialization {
                message: "test error".to_string(),
            },
            ReplyError::EndpointNotFound,
            ReplyError::MaybeDelivered,
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
