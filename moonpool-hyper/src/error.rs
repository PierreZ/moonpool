//! Errors surfaced by the client channels.

use thiserror::Error;

/// What a channel reports to its caller.
///
/// Payloads are `String`s rather than source errors so the type stays `Clone`:
/// one connection failure can be reported to several waiting callers, and the
/// underlying `hyper::Error` and `io::Error` are neither `Clone` nor useful to
/// match on here. This mirrors moonpool-transport's `PeerError`.
#[derive(Error, Debug, Clone)]
pub enum ChannelError {
    /// A connection attempt failed: TCP connect, the connect timeout, or the
    /// h2 handshake.
    #[error("connection to {destination} failed: {reason}")]
    Connect {
        /// Address the channel was trying to reach.
        destination: String,
        /// What went wrong, as reported by the provider or by hyper.
        reason: String,
    },

    /// The channel gave up: `max_connection_failures` consecutive attempts
    /// failed. The channel stays in this state, so build a new one to retry.
    #[error("gave up on {destination} after {failures} consecutive connection failures")]
    GaveUp {
        /// Address the channel was trying to reach.
        destination: String,
        /// How many consecutive attempts failed.
        failures: u32,
    },

    /// The h2 connection is closed. A [`ReconnectingChannel`] absorbs this and
    /// reconnects; a bare [`H2Channel`] surfaces it, since it has no way to
    /// obtain a new connection.
    ///
    /// [`ReconnectingChannel`]: crate::ReconnectingChannel
    /// [`H2Channel`]: crate::H2Channel
    #[error("connection closed: {0}")]
    Closed(String),

    /// `call` was invoked while the channel had no live connection. Tower's
    /// contract is to await `poll_ready` first; this is the error rather than
    /// a panic so a racing connection loss stays recoverable.
    #[error("channel is not ready: poll_ready must report Ready(Ok) before call")]
    NotReady,

    /// The request failed on an established connection (stream reset, the
    /// connection went away mid-request, protocol error).
    #[error("request failed: {0}")]
    Request(String),
}
