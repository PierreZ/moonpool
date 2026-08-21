//! One established h2 connection, as a tower service.

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::{Body, Incoming};
use hyper::client::conn::http2::SendRequest;
use hyper::{Request, Response};

use crate::error::ChannelError;

/// Response future returned by [`H2Channel`] and
/// [`ReconnectingChannel`](crate::ReconnectingChannel).
///
/// Boxed because hyper's `send_request` future is unnameable.
pub type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<Response<Incoming>, ChannelError>> + Send>>;

/// A tower [`Service`](tower_service::Service) over a single h2 connection.
///
/// Cloning shares the connection, so every clone multiplexes its requests onto
/// the same stream. This is the sender half of a hyper handshake: the caller is
/// responsible for driving the matching connection future, without which no
/// request makes progress.
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
    pub fn new(sender: SendRequest<B>) -> Self {
        Self { sender }
    }

    /// Whether the underlying connection has been closed.
    ///
    /// This is a hint in the same sense as hyper's own
    /// [`SendRequest::is_closed`]: it reports a connection already known to be
    /// gone, but a request can still fail on a connection that looks live.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
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
        // ready channel can still refuse a request when the peer's stream
        // limit is reached.
        self.sender
            .poll_ready(cx)
            .map_err(|e| ChannelError::Closed(e.to_string()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let request = self.sender.send_request(req);
        Box::pin(async move {
            request
                .await
                .map_err(|e| ChannelError::Request(e.to_string()))
        })
    }
}
