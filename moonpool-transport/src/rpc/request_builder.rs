//! Request builder for fluent RPC calls with timeout support.
//!
//! Provides a builder pattern for sending RPC requests with optional timeout,
//! eliminating the need for manual timeout wrapping.
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool_transport::{JsonCodec, TimeProvider};
//! use std::time::Duration;
//!
//! // Instead of:
//! let future = transport.call::<_, Response, _>(&endpoint, request, JsonCodec)?;
//! match time.timeout(Duration::from_secs(5), future).await {
//!     Ok(Ok(Ok(resp))) => { /* success */ }
//!     _ => { /* error */ }
//! }
//!
//! // Use the builder:
//! let resp: Response = transport
//!     .request(&endpoint, request)
//!     .with_timeout(Duration::from_secs(5), &time)
//!     .send(JsonCodec)
//!     .await?;
//! ```

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::MessagingError;
use crate::rpc::net_transport::NetTransport;
use crate::rpc::reply_error::ReplyError;
use crate::{Endpoint, MessageCodec, NetworkProvider, RandomProvider, TaskProvider, TimeProvider};

/// Error type for request builder operations.
#[derive(Debug)]
pub enum RpcError {
    /// Failed to send the request.
    SendFailed(MessagingError),
    /// The request timed out.
    Timeout,
    /// The reply promise was broken (server didn't respond).
    BrokenPromise,
    /// The server returned an error.
    ReplyError(ReplyError),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::SendFailed(e) => write!(f, "failed to send request: {}", e),
            RpcError::Timeout => write!(f, "request timed out"),
            RpcError::BrokenPromise => write!(f, "server did not respond (broken promise)"),
            RpcError::ReplyError(e) => write!(f, "server error: {:?}", e),
        }
    }
}

impl std::error::Error for RpcError {}

impl From<MessagingError> for RpcError {
    fn from(e: MessagingError) -> Self {
        RpcError::SendFailed(e)
    }
}

/// Builder for RPC requests with optional timeout.
///
/// Created via [`NetTransport::request`].
pub struct RequestBuilder<'a, Req, N, T, TP, R, Time = ()>
where
    Req: Serialize,
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    R: RandomProvider + Clone + 'static,
{
    transport: &'a NetTransport<N, T, TP, R>,
    destination: &'a Endpoint,
    request: Req,
    timeout: Option<(Duration, Time)>,
}

impl<'a, Req, N, T, TP, R> RequestBuilder<'a, Req, N, T, TP, R, ()>
where
    Req: Serialize,
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    R: RandomProvider + Clone + 'static,
{
    /// Create a new request builder.
    pub(crate) fn new(
        transport: &'a NetTransport<N, T, TP, R>,
        destination: &'a Endpoint,
        request: Req,
    ) -> Self {
        Self {
            transport,
            destination,
            request,
            timeout: None,
        }
    }

    /// Set a timeout for this request.
    ///
    /// The request will fail with `RpcError::Timeout` if no response is
    /// received within the specified duration.
    ///
    /// # Arguments
    ///
    /// * `duration` - Maximum time to wait for a response
    /// * `time` - Time provider for the timeout
    pub fn with_timeout<Time: TimeProvider>(
        self,
        duration: Duration,
        time: &'a Time,
    ) -> RequestBuilder<'a, Req, N, T, TP, R, &'a Time> {
        RequestBuilder {
            transport: self.transport,
            destination: self.destination,
            request: self.request,
            timeout: Some((duration, time)),
        }
    }

    /// Send the request without a timeout.
    ///
    /// Returns a future that resolves when the response is received.
    /// Without a timeout, this may wait indefinitely if the server doesn't respond.
    ///
    /// # Type Parameters
    ///
    /// * `Resp` - The expected response type
    /// * `C` - The codec to use for serialization
    pub async fn send<Resp, C>(self, codec: C) -> Result<Resp, RpcError>
    where
        Resp: DeserializeOwned + 'static,
        C: MessageCodec,
    {
        let future = self
            .transport
            .call::<Req, Resp, C>(self.destination, self.request, codec)?;

        // ReplyFuture returns Result<Resp, ReplyError>
        match future.await {
            Ok(resp) => Ok(resp),
            Err(e) => Err(RpcError::ReplyError(e)),
        }
    }
}

impl<'a, Req, N, T, TP, R, Time> RequestBuilder<'a, Req, N, T, TP, R, &'a Time>
where
    Req: Serialize,
    N: NetworkProvider + Clone + 'static,
    T: TimeProvider + Clone + 'static,
    TP: TaskProvider + Clone + 'static,
    R: RandomProvider + Clone + 'static,
    Time: TimeProvider,
{
    /// Send the request with the configured timeout.
    ///
    /// Returns the response or an error if the request fails or times out.
    ///
    /// # Type Parameters
    ///
    /// * `Resp` - The expected response type
    /// * `C` - The codec to use for serialization
    pub async fn send<Resp, C>(self, codec: C) -> Result<Resp, RpcError>
    where
        Resp: DeserializeOwned + 'static,
        C: MessageCodec,
    {
        let future = self
            .transport
            .call::<Req, Resp, C>(self.destination, self.request, codec)?;

        let (duration, time) = self.timeout.expect("timeout should be set");

        // TimeProvider::timeout returns SimulationResult<Result<T, ()>>
        // which is Result<Result<T, ()>, SimulationError>
        // where T = ReplyFuture::Output = Result<Resp, ReplyError>
        // Full type: Result<Result<Result<Resp, ReplyError>, ()>, SimulationError>
        match time.timeout(duration, future).await {
            Ok(Ok(Ok(resp))) => Ok(resp),
            Ok(Ok(Err(e))) => Err(RpcError::ReplyError(e)),
            Ok(Err(())) => Err(RpcError::Timeout),
            Err(_) => Err(RpcError::Timeout), // SimulationError treated as timeout
        }
    }
}
