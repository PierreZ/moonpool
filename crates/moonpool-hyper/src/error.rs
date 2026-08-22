//! Errors surfaced by the client channels.

use thiserror::Error;

/// What a channel reports to its caller.
///
/// Payloads are strings because the provider and hyper errors have different
/// concrete types. As a `Send + Sync` [`std::error::Error`], this converts into
/// the boxed error tonic's generated clients require.
#[derive(Error, Debug)]
pub enum ChannelError {
    /// The TCP connect failed.
    #[error("connect to {addr} failed: {detail}")]
    Connect {
        /// Address the channel was trying to reach.
        addr: String,
        /// What the network provider reported.
        detail: String,
    },

    /// The attempt ran out of `connection_timeout` before a connection was
    /// established. Distinct from [`Connect`](ChannelError::Connect): a peer
    /// that is gone usually times out, while one that is up but refusing
    /// answers immediately.
    #[error("connect to {addr} timed out")]
    ConnectTimeout {
        /// Address the channel was trying to reach.
        addr: String,
    },

    /// TCP connected but the h2 handshake failed.
    #[error("h2 handshake with {addr} failed: {detail}")]
    Handshake {
        /// Address the channel was trying to reach.
        addr: String,
        /// What hyper reported.
        detail: String,
    },

    /// The request failed on an established connection (stream reset, the
    /// connection went away mid-request, protocol error).
    #[error("request failed: {detail}")]
    Request {
        /// What hyper reported.
        detail: String,
    },

    /// The h2 connection is closed. A
    /// [`ReconnectingChannel`](crate::ReconnectingChannel) absorbs this and
    /// reconnects; a bare [`H2Channel`](crate::H2Channel) surfaces it, since it
    /// has no way to obtain a new connection.
    #[error("connection closed")]
    Closed,

    /// `call` was invoked without a successful `poll_ready` before it. Tower's
    /// contract is to await readiness first; this is an error rather than a
    /// panic so a connection lost in the race stays recoverable.
    #[error("channel is not ready: poll_ready must report Ready(Ok) before call")]
    NotReady,

    /// `max_connection_failures` consecutive attempts failed. The channel stays
    /// in this state, so build a new one to retry.
    #[error("gave up after {failures} consecutive connection failures")]
    ExhaustedRetries {
        /// How many consecutive attempts failed.
        failures: u32,
    },
}
