//! Call outcomes: the failure reason, kept separate from what it proves
//! about execution.

use crate::codec::CodecId;
use crate::protocol::{MethodId, SchemaVersion, WireError};

/// What a failure proves about whether the server ran the handler for the
/// failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Execution {
    /// The request was never handed to a handler: it was refused before
    /// admission (by this runtime or by the server) or never left this
    /// process. Retrying cannot duplicate a previous execution of *this*
    /// attempt.
    NotAdmitted,
    /// The handler may or may not have run (the request may have been
    /// admitted, and no outcome came back). Retrying may execute it twice.
    MaybeExecuted,
    /// The handler ran: it produced an outcome this side could not use.
    Executed,
}

/// Why one request attempt did not produce a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorReason {
    /// No live endpoint holds this token: the receiver was dropped (or never
    /// existed). Terminal for this reference.
    EndpointNotFound,
    /// The reference names an earlier incarnation of the runtime at that
    /// address (a restarted process). Terminal for this reference.
    StaleIncarnation,
    /// The endpoint serves a different method.
    MethodMismatch {
        /// The method the caller invoked.
        called: MethodId,
        /// The method the endpoint was registered for.
        registered: MethodId,
    },
    /// The endpoint serves a different version of the method's contract.
    SchemaMismatch {
        /// The contract version the caller encoded with.
        called: SchemaVersion,
        /// The contract version the endpoint was registered with.
        registered: SchemaVersion,
    },
    /// The receiving side expects another body codec; the body was never
    /// decoded.
    CodecMismatch {
        /// The codec the body was encoded with.
        sent: CodecId,
        /// The codec the receiving side accepts.
        expected: CodecId,
    },
    /// The server could not decode the request as its registered type.
    MalformedRequest,
    /// Admission was refused for capacity: the endpoint's queue, this
    /// runtime's pending-call budget, a connection's request queue or the
    /// connection budget was full.
    Overloaded,
    /// The encoded request exceeds the frame limit; nothing was sent.
    FrameTooLarge {
        /// Encoded envelope size.
        size: u64,
        /// Configured limit.
        limit: u32,
    },
    /// The request could not be encoded; nothing was sent.
    Encode(String),
    /// No session to the address could be established (connect, upgrade or
    /// handshake failed); nothing was sent.
    ConnectFailed(String),
    /// The session carrying the attempt closed before an outcome arrived.
    Disconnected,
    /// The caller's deadline passed before an outcome arrived.
    Timeout,
    /// The server admitted the request, then dropped its reply handle
    /// without replying.
    BrokenPromise,
    /// The handler replied, but the reply exceeded the session's frame
    /// limit (the smaller of the two peers' limits).
    ReplyTooLarge,
    /// The handler replied, but its codec failed to encode the reply.
    ReplyEncodeFailed,
    /// A reply arrived but did not decode as the expected reply type.
    MalformedReply(String),
    /// The local RPC runtime is gone (its driver was dropped).
    Shutdown,
    /// Registration needs a listening runtime.
    NotListening,
    /// Another live receiver already holds this well-known id.
    AlreadyRegistered,
    /// A bootstrap hostname did not resolve; nothing was sent.
    LookupFailed(String),
    /// The failure monitor reported the endpoint failed for the caller's
    /// sustained-failure bound, or failed permanently, before a reply came.
    /// An observation, not proof that the server is gone.
    PeerFailed,
}

impl std::fmt::Display for ErrorReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndpointNotFound => f.write_str("endpoint not found"),
            Self::StaleIncarnation => {
                f.write_str("endpoint belongs to a previous runtime incarnation")
            }
            Self::MethodMismatch { called, registered } => {
                write!(
                    f,
                    "method mismatch: called {called}, endpoint serves {registered}"
                )
            }
            Self::SchemaMismatch { called, registered } => {
                write!(
                    f,
                    "schema mismatch: called {called}, endpoint serves {registered}"
                )
            }
            Self::CodecMismatch { sent, expected } => {
                write!(f, "codec mismatch: sent {sent}, expected {expected}")
            }
            Self::MalformedRequest => f.write_str("the server could not decode the request"),
            Self::Overloaded => f.write_str("overloaded"),
            Self::FrameTooLarge { size, limit } => {
                write!(
                    f,
                    "request of {size} bytes exceeds the {limit}-byte frame limit"
                )
            }
            Self::Encode(detail) => write!(f, "request encoding failed: {detail}"),
            Self::ConnectFailed(detail) => write!(f, "connect failed: {detail}"),
            Self::Disconnected => f.write_str("disconnected before an outcome arrived"),
            Self::Timeout => f.write_str("deadline passed before an outcome arrived"),
            Self::BrokenPromise => f.write_str("broken promise: the server dropped the reply"),
            Self::ReplyTooLarge => f.write_str("the reply exceeded the frame limit"),
            Self::ReplyEncodeFailed => f.write_str("the reply could not be encoded"),
            Self::MalformedReply(detail) => write!(f, "malformed reply: {detail}"),
            Self::Shutdown => f.write_str("the RPC runtime has shut down"),
            Self::NotListening => f.write_str("the RPC runtime is not listening"),
            Self::AlreadyRegistered => f.write_str("the well-known id is already registered"),
            Self::LookupFailed(detail) => write!(f, "lookup failed: {detail}"),
            Self::PeerFailed => f.write_str("the endpoint was observed failed"),
        }
    }
}

/// The one error every RPC operation returns: a [reason](ErrorReason) and
/// what it proves about [execution](Execution).
///
/// The two are separate on purpose. The same reason can carry different
/// knowledge: a disconnect before the request frame left this process is
/// [`Execution::NotAdmitted`]; after, [`Execution::MaybeExecuted`]. Every
/// failure path reports the strongest knowledge it honestly has, never more.
///
/// A caller that drops the call future (or wraps it in its own timeout) is
/// cancelling, not failing: no `RpcError` is produced, and the attempt must
/// be treated as [`Execution::MaybeExecuted`]. Use
/// [`ServiceClient::try_get_reply_within`](crate::ServiceClient::try_get_reply_within)
/// to get a timeout with execution knowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RpcError {
    reason: ErrorReason,
    execution: Execution,
}

impl RpcError {
    /// Build an error from a reason and the execution knowledge it carries.
    #[must_use]
    pub const fn new(reason: ErrorReason, execution: Execution) -> Self {
        Self { reason, execution }
    }

    /// A refusal that proves the attempt never reached a handler.
    #[must_use]
    pub const fn not_admitted(reason: ErrorReason) -> Self {
        Self::new(reason, Execution::NotAdmitted)
    }

    /// Why the attempt failed.
    #[must_use]
    pub fn reason(&self) -> &ErrorReason {
        &self.reason
    }

    /// What the failure proves about execution.
    #[must_use]
    pub fn execution(&self) -> Execution {
        self.execution
    }

    /// Weaken the execution knowledge to at least `floor`: a rejection of
    /// one attempt proves nothing about an earlier attempt that left.
    pub(crate) fn at_least(mut self, floor: Execution) -> Self {
        if self.execution == Execution::NotAdmitted && floor != Execution::NotAdmitted {
            self.execution = floor;
        }
        self
    }

    /// Whether retrying the same reference cannot succeed.
    #[must_use]
    pub fn is_terminal_for_reference(&self) -> bool {
        matches!(
            self.reason,
            ErrorReason::EndpointNotFound
                | ErrorReason::StaleIncarnation
                | ErrorReason::MethodMismatch { .. }
                | ErrorReason::SchemaMismatch { .. }
                | ErrorReason::CodecMismatch { .. }
        )
    }

    pub(crate) fn from_wire(error: WireError, called: CallIdentity) -> Self {
        let reason = match error {
            WireError::EndpointNotFound => ErrorReason::EndpointNotFound,
            WireError::StaleIncarnation => ErrorReason::StaleIncarnation,
            WireError::MethodMismatch { registered } => ErrorReason::MethodMismatch {
                called: called.method,
                registered,
            },
            WireError::SchemaMismatch { registered } => ErrorReason::SchemaMismatch {
                called: called.schema,
                registered,
            },
            WireError::CodecMismatch { registered } => ErrorReason::CodecMismatch {
                sent: called.codec,
                expected: registered,
            },
            WireError::MalformedRequest => ErrorReason::MalformedRequest,
            WireError::Overloaded => ErrorReason::Overloaded,
            WireError::BrokenPromise => {
                return Self::new(ErrorReason::BrokenPromise, Execution::MaybeExecuted);
            }
            WireError::ReplyTooLarge => {
                return Self::new(ErrorReason::ReplyTooLarge, Execution::Executed);
            }
            WireError::ReplyEncodeFailed => {
                return Self::new(ErrorReason::ReplyEncodeFailed, Execution::Executed);
            }
        };
        // Every other server-side rejection happens before admission.
        Self::not_admitted(reason)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?})", self.reason, self.execution)
    }
}

impl std::error::Error for RpcError {}

/// What a caller claimed about the endpoint it called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallIdentity {
    pub(crate) method: MethodId,
    pub(crate) schema: SchemaVersion,
    pub(crate) codec: CodecId,
}

#[cfg(test)]
mod tests {
    use super::{CallIdentity, ErrorReason, Execution, RpcError};
    use crate::codec::CodecId;
    use crate::protocol::{MethodId, SchemaVersion, WireError};

    #[test]
    fn server_rejections_map_to_honest_execution_knowledge() {
        let called = CallIdentity {
            method: MethodId::new(1),
            schema: SchemaVersion::new(1),
            codec: CodecId::PROST,
        };
        let knowledge = |error| RpcError::from_wire(error, called).execution();
        assert_eq!(
            knowledge(WireError::EndpointNotFound),
            Execution::NotAdmitted
        );
        assert_eq!(knowledge(WireError::Overloaded), Execution::NotAdmitted);
        assert_eq!(
            knowledge(WireError::MalformedRequest),
            Execution::NotAdmitted
        );
        assert_eq!(
            knowledge(WireError::BrokenPromise),
            Execution::MaybeExecuted
        );
        assert_eq!(knowledge(WireError::ReplyTooLarge), Execution::Executed);
        assert_eq!(knowledge(WireError::ReplyEncodeFailed), Execution::Executed);
        let stale = RpcError::from_wire(WireError::StaleIncarnation, called);
        assert!(stale.is_terminal_for_reference());
        assert_eq!(stale.reason(), &ErrorReason::StaleIncarnation);
        assert!(
            !RpcError::new(ErrorReason::Disconnected, Execution::MaybeExecuted)
                .is_terminal_for_reference()
        );
    }
}
