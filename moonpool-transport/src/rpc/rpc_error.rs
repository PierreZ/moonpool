//! Unified error type for RPC operations.
//!
//! `RpcError` combines `MessagingError` (send-side errors) and `ReplyError`
//! (receive-side errors) into a single type for ergonomic RPC calls.
//!
//! # Example
//!
//! ```rust,ignore
//! // Before: manual error handling
//! let future = send_request(&transport, &endpoint, req, codec)?; // MessagingError
//! let result = future.await?; // ReplyError
//!
//! // After: unified RpcError
//! let result = bound_client.method(req).await?; // RpcError
//! ```

use crate::error::MessagingError;
use crate::rpc::ReplyError;

/// Unified error type for RPC operations.
///
/// This error type wraps both send-side errors (`MessagingError`) and
/// receive-side errors (`ReplyError`) to provide a single error type
/// for bound client trait methods.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// Error occurred while sending the request.
    ///
    /// This includes serialization failures, network errors, and
    /// transport-level issues.
    #[error("messaging error")]
    Messaging(#[from] MessagingError),

    /// Error occurred while waiting for or processing the response.
    ///
    /// This includes broken promises, connection failures, timeouts,
    /// and deserialization errors.
    #[error("reply error")]
    Reply(#[from] ReplyError),
}

impl RpcError {
    /// Returns true if this error indicates the request may or may not
    /// have been delivered (FDB error 1030: `request_maybe_delivered`).
    #[must_use]
    pub fn is_maybe_delivered(&self) -> bool {
        matches!(self, RpcError::Reply(ReplyError::MaybeDelivered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UID;
    use std::error::Error;

    #[test]
    fn test_rpc_error_display() {
        let messaging_err = RpcError::Messaging(MessagingError::EndpointNotFound {
            token: UID::new(1, 2),
        });
        assert!(messaging_err.to_string().contains("messaging error"));

        let reply_err = RpcError::Reply(ReplyError::BrokenPromise);
        assert!(reply_err.to_string().contains("reply error"));
    }

    #[test]
    fn test_rpc_error_from_messaging() {
        let err = MessagingError::MissingLocalAddress;
        let rpc_err: RpcError = err.into();
        assert!(matches!(rpc_err, RpcError::Messaging(_)));
    }

    #[test]
    fn test_rpc_error_from_reply() {
        let err = ReplyError::Timeout;
        let rpc_err: RpcError = err.into();
        assert!(matches!(rpc_err, RpcError::Reply(_)));
    }

    #[test]
    fn test_rpc_error_source() {
        let rpc_err = RpcError::Reply(ReplyError::ConnectionFailed);
        assert!(rpc_err.source().is_some());
    }
}
