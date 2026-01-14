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
#[derive(Debug)]
pub enum RpcError {
    /// Error occurred while sending the request.
    ///
    /// This includes serialization failures, network errors, and
    /// transport-level issues.
    Messaging(MessagingError),

    /// Error occurred while waiting for or processing the response.
    ///
    /// This includes broken promises, connection failures, timeouts,
    /// and deserialization errors.
    Reply(ReplyError),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Messaging(e) => write!(f, "messaging error: {}", e),
            RpcError::Reply(e) => write!(f, "reply error: {}", e),
        }
    }
}

impl std::error::Error for RpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RpcError::Messaging(e) => Some(e),
            RpcError::Reply(e) => Some(e),
        }
    }
}

impl From<MessagingError> for RpcError {
    fn from(err: MessagingError) -> Self {
        RpcError::Messaging(err)
    }
}

impl From<ReplyError> for RpcError {
    fn from(err: ReplyError) -> Self {
        RpcError::Reply(err)
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
        let err = MessagingError::TransportClosed;
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
