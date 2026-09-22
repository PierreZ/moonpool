//! Call outcomes: the failure reason, kept separate from what it proves
//! about execution.

use thiserror::Error;

use crate::codec::CodecId;
use crate::protocol::{MethodId, SchemaId, WireError};

/// What an error proves about whether the server ran the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Execution {
    /// The handler did not run for this attempt: the request was refused
    /// before admission or never left this process.
    NotExecuted,
    /// The handler may or may not have run. Retrying may execute it twice.
    MaybeExecuted,
    /// The handler ran (it produced a reply this side could not use).
    Executed,
}

/// Why one request attempt did not produce a reply.
///
/// Each variant is a *reason*; [`execution`](Self::execution) says what it
/// proves. A timeout the caller applies around a call (or dropping the call
/// future) is not an `RpcError` at all: it is cancellation, and it is always
/// [`Execution::MaybeExecuted`] once the request may have been sent.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RpcError {
    /// No live endpoint holds this token: the receiver was dropped (or never
    /// existed). Terminal for this reference; the server did not run it.
    #[error("endpoint not found")]
    EndpointNotFound,
    /// The reference names an earlier incarnation of the transport at that
    /// address (a restarted process). Terminal for this reference.
    #[error("endpoint belongs to a previous transport incarnation")]
    StaleIncarnation,
    /// The endpoint serves a different method.
    #[error("method mismatch: called {called}, endpoint serves {registered}")]
    MethodMismatch {
        /// The method the caller invoked.
        called: MethodId,
        /// The method the endpoint was registered for.
        registered: MethodId,
    },
    /// The endpoint serves a different version of the method's contract.
    #[error("schema mismatch: called {called}, endpoint serves {registered}")]
    SchemaMismatch {
        /// The contract the caller encoded with.
        called: SchemaId,
        /// The contract the endpoint was registered with.
        registered: SchemaId,
    },
    /// The endpoint (or, for a reply, the caller) expects another codec.
    /// The body was never decoded.
    #[error("codec mismatch: sent {sent}, expected {expected}")]
    CodecMismatch {
        /// The codec the body was encoded with.
        sent: CodecId,
        /// The codec the receiving side accepts.
        expected: CodecId,
    },
    /// The server could not decode the request as its registered type.
    #[error("the server could not decode the request")]
    MalformedRequest,
    /// Admission was refused for capacity: the endpoint's queue, this
    /// runtime's pending-call budget, a connection's request queue or the
    /// connection budget was full.
    #[error("overloaded")]
    Overloaded,
    /// The encoded request exceeds the frame limit; nothing was sent.
    #[error("request of {size} bytes exceeds the {limit}-byte frame limit")]
    FrameTooLarge {
        /// Encoded size.
        size: u64,
        /// Configured limit.
        limit: u32,
    },
    /// The request could not be encoded; nothing was sent.
    #[error("request encoding failed: {0}")]
    Encode(String),
    /// No connection to the address could be established; nothing was sent.
    #[error("connect failed: {0}")]
    ConnectFailed(String),
    /// The connection carrying the attempt closed before a reply arrived.
    ///
    /// `transmitted` is whether the request frame had started to leave this
    /// process; when it had, the server may have run the handler.
    #[error("disconnected (request transmitted: {transmitted})")]
    Disconnected {
        /// Whether the request frame had begun transmission.
        transmitted: bool,
    },
    /// The server admitted the request, then dropped its reply handle
    /// without replying.
    #[error("broken promise: the server dropped the reply handle")]
    BrokenPromise,
    /// The handler replied, but the reply exceeded the frame limit.
    #[error("the reply exceeded the frame limit")]
    ReplyTooLarge,
    /// A reply arrived but did not decode as the expected reply type.
    #[error("malformed reply: {0}")]
    MalformedReply(String),
    /// The local RPC runtime is gone (its driver was dropped).
    #[error("the RPC runtime has shut down")]
    Shutdown,
    /// Registration needs a listening transport.
    #[error("the RPC runtime is not listening")]
    NotListening,
}

impl RpcError {
    /// What this error proves about execution of the failed attempt.
    #[must_use]
    pub fn execution(&self) -> Execution {
        match self {
            Self::EndpointNotFound
            | Self::StaleIncarnation
            | Self::MethodMismatch { .. }
            | Self::SchemaMismatch { .. }
            | Self::CodecMismatch { .. }
            | Self::MalformedRequest
            | Self::Overloaded
            | Self::FrameTooLarge { .. }
            | Self::Encode(_)
            | Self::ConnectFailed(_)
            | Self::NotListening
            | Self::Disconnected { transmitted: false } => Execution::NotExecuted,
            Self::Disconnected { transmitted: true } | Self::BrokenPromise | Self::Shutdown => {
                Execution::MaybeExecuted
            }
            Self::ReplyTooLarge | Self::MalformedReply(_) => Execution::Executed,
        }
    }

    /// Whether the error is terminal for the reference it was raised for:
    /// retrying the same reference cannot succeed.
    #[must_use]
    pub fn is_terminal_for_reference(&self) -> bool {
        matches!(
            self,
            Self::EndpointNotFound
                | Self::StaleIncarnation
                | Self::MethodMismatch { .. }
                | Self::SchemaMismatch { .. }
                | Self::CodecMismatch { .. }
        )
    }

    pub(crate) fn from_wire(error: WireError, called: &CallIdentity) -> Self {
        match error {
            WireError::EndpointNotFound => Self::EndpointNotFound,
            WireError::StaleIncarnation => Self::StaleIncarnation,
            WireError::MethodMismatch { registered } => Self::MethodMismatch {
                called: called.method,
                registered,
            },
            WireError::SchemaMismatch { registered } => Self::SchemaMismatch {
                called: called.schema,
                registered,
            },
            WireError::CodecMismatch { registered } => Self::CodecMismatch {
                sent: called.codec,
                expected: registered,
            },
            WireError::MalformedRequest => Self::MalformedRequest,
            WireError::Overloaded => Self::Overloaded,
            WireError::BrokenPromise => Self::BrokenPromise,
            WireError::ReplyTooLarge => Self::ReplyTooLarge,
        }
    }
}

/// What a caller claimed about the endpoint it called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallIdentity {
    pub(crate) method: MethodId,
    pub(crate) schema: SchemaId,
    pub(crate) codec: CodecId,
}

#[cfg(test)]
mod tests {
    use super::{Execution, RpcError};

    #[test]
    fn execution_knowledge_is_separate_from_the_reason() {
        assert_eq!(
            RpcError::Disconnected { transmitted: false }.execution(),
            Execution::NotExecuted
        );
        assert_eq!(
            RpcError::Disconnected { transmitted: true }.execution(),
            Execution::MaybeExecuted
        );
        assert_eq!(RpcError::Overloaded.execution(), Execution::NotExecuted);
        assert_eq!(
            RpcError::BrokenPromise.execution(),
            Execution::MaybeExecuted
        );
        assert_eq!(
            RpcError::MalformedReply(String::new()).execution(),
            Execution::Executed
        );
        assert!(RpcError::StaleIncarnation.is_terminal_for_reference());
        assert!(!RpcError::Disconnected { transmitted: true }.is_terminal_for_reference());
    }
}
