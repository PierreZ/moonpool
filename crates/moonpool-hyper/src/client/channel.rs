//! One established h2 connection, as a tower service.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::{Body, Incoming};
use hyper::client::conn::http2::SendRequest;
use hyper::{Request, Response};
use tracing::instrument;

use crate::error::ChannelError;

/// Response future of [`H2Channel`] and
/// [`ReconnectingChannel`](crate::ReconnectingChannel).
///
/// A named type wrapping a boxed future, because hyper's `send_request` returns
/// an unnameable `impl Future` and a service's `Future` associated type has to
/// be named.
pub struct ResponseFuture {
    inner: Pin<Box<dyn Future<Output = Result<Response<Incoming>, ChannelError>> + Send>>,
}

impl ResponseFuture {
    /// Wrap a future that will produce the response.
    pub(crate) fn new(
        future: impl Future<Output = Result<Response<Incoming>, ChannelError>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(future),
        }
    }

    /// A future that fails immediately, for a request that never reached a
    /// connection.
    pub(crate) fn failed(error: ChannelError) -> Self {
        Self::new(std::future::ready(Err(error)))
    }
}

impl fmt::Debug for ResponseFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseFuture").finish_non_exhaustive()
    }
}

impl Future for ResponseFuture {
    type Output = Result<Response<Incoming>, ChannelError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().inner.as_mut().poll(cx)
    }
}

/// A tower [`Service`](tower_service::Service) over a single h2 connection.
///
/// Cloning shares the connection, so every clone multiplexes its requests onto
/// the same stream. This is the sender half of a hyper handshake: the caller
/// drives the matching connection future, without which no request makes
/// progress.
///
/// The channel never reconnects. Once the connection is gone, `poll_ready`
/// reports [`ChannelError::Closed`] forever; use
/// [`ReconnectingChannel`](crate::ReconnectingChannel) to have that handled.
#[derive(Debug)]
pub struct H2Channel<B> {
    sender: SendRequest<B>,
}

// Manual: the derive would demand `B: Clone`, but the body type is only ever a
// type parameter of the sender, which clones unconditionally.
impl<B> Clone for H2Channel<B> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<B> H2Channel<B> {
    /// Wrap the sender half returned by a hyper h2 handshake.
    #[must_use]
    #[instrument(skip_all)]
    pub fn new(sender: SendRequest<B>) -> Self {
        Self { sender }
    }

    /// Whether the underlying connection has been closed.
    ///
    /// A hint, in the same sense as hyper's own [`SendRequest::is_closed`]: it
    /// reports a connection already known to be gone, but a request can still
    /// fail on one that looks live.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// Whether the connection can currently accept a request.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.sender.is_ready()
    }
}

impl<B> tower_service::Service<Request<B>> for H2Channel<B>
where
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>>,
{
    type Response = Response<Incoming>;
    type Error = ChannelError;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // hyper ignores the context here and only reports whether the
        // connection is closed: an h2 sender applies no backpressure, so a
        // ready channel can still refuse a request once the peer's concurrent
        // stream limit is reached.
        self.sender.poll_ready(cx).map_err(|_| ChannelError::Closed)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let request = self.sender.send_request(req);
        ResponseFuture::new(async move {
            request.await.map_err(|error| ChannelError::Request {
                detail: error.to_string(),
            })
        })
    }
}
