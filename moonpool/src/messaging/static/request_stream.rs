//! RequestStream: Server-side stream for receiving typed requests.
//!
//! A RequestStream receives incoming requests and provides `ReplyPromise` handles
//! for each request, allowing the server to respond.
//!
//! # FDB Reference
//! Based on `RequestStream<T>` from fdbrpc.h.
//!
//! # Example
//!
//! ```rust,ignore
//! // Server loop
//! let stream = RequestStream::<PingRequest, _, JsonCodec>::new(endpoint, codec);
//!
//! loop {
//!     if let Some((request, reply)) = stream.recv(&transport).await {
//!         // Handle request and send reply
//!         reply.send(PingResponse { pong: request.ping });
//!     }
//! }
//! ```

use std::rc::Rc;

use moonpool_foundation::Endpoint;
use moonpool_traits::MessageCodec;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::net_notified_queue::NetNotifiedQueue;
use super::reply_promise::ReplyPromise;

/// Envelope wrapping a request with its reply endpoint.
///
/// When a client sends a request, it includes the endpoint where it's
/// listening for the response. This envelope bundles both.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RequestEnvelope<T> {
    /// The actual request payload.
    pub request: T,

    /// Endpoint where the client is listening for the response.
    pub reply_to: Endpoint,
}

/// Stream for receiving typed requests with reply promises.
///
/// The server uses this to receive incoming requests. Each received request
/// comes with a `ReplyPromise` that must be fulfilled.
pub struct RequestStream<Req, C: MessageCodec> {
    /// Queue for incoming request envelopes.
    queue: Rc<NetNotifiedQueue<RequestEnvelope<Req>, C>>,

    /// Codec for serializing responses.
    codec: C,
}

impl<Req, C> RequestStream<Req, C>
where
    Req: DeserializeOwned + 'static,
    C: MessageCodec,
{
    /// Create a new request stream with the given endpoint and codec.
    pub fn new(endpoint: Endpoint, codec: C) -> Self {
        Self {
            queue: Rc::new(NetNotifiedQueue::new(endpoint, codec.clone())),
            codec,
        }
    }

    /// Get the endpoint for this stream.
    ///
    /// Clients use this endpoint to send requests.
    pub fn endpoint(&self) -> &Endpoint {
        self.queue.endpoint()
    }

    /// Get a reference to the internal queue for registration.
    ///
    /// This is used to register the stream with FlowTransport.
    pub fn queue(&self) -> Rc<NetNotifiedQueue<RequestEnvelope<Req>, C>> {
        self.queue.clone()
    }

    /// Receive the next request with its reply promise.
    ///
    /// Returns `None` if the stream is closed.
    ///
    /// The `sender` function is used to send the reply back to the client.
    /// Typically this is provided by the FlowTransport.
    pub async fn recv<F, Resp>(&self, sender: F) -> Option<(Req, ReplyPromise<Resp, C>)>
    where
        Resp: Serialize,
        F: FnOnce(&Endpoint, &[u8]) + 'static,
    {
        let envelope = self.queue.recv().await?;

        let reply = ReplyPromise::new(envelope.reply_to, self.codec.clone(), sender);

        Some((envelope.request, reply))
    }

    /// Try to receive a request without blocking.
    ///
    /// Returns `None` if no request is available.
    pub fn try_recv<F, Resp>(&self, sender: F) -> Option<(Req, ReplyPromise<Resp, C>)>
    where
        Resp: Serialize,
        F: FnOnce(&Endpoint, &[u8]) + 'static,
    {
        let envelope = self.queue.try_recv()?;

        let reply = ReplyPromise::new(envelope.reply_to, self.codec.clone(), sender);

        Some((envelope.request, reply))
    }

    /// Check if the stream is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Get the number of pending requests.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Close the stream.
    pub fn close(&self) {
        self.queue.close();
    }

    /// Check if the stream is closed.
    pub fn is_closed(&self) -> bool {
        self.queue.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use moonpool_foundation::{NetworkAddress, UID};
    use moonpool_traits::JsonCodec;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::MessageReceiver;

    fn test_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4500);
        Endpoint::new(addr, UID::new(1, 1))
    }

    fn reply_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4501);
        Endpoint::new(addr, UID::new(2, 2))
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PingRequest {
        seq: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PingResponse {
        seq: u32,
    }

    #[test]
    fn test_request_stream_endpoint() {
        let endpoint = test_endpoint();
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(endpoint.clone(), JsonCodec);

        assert_eq!(stream.endpoint().token, endpoint.token);
    }

    #[test]
    fn test_request_envelope_serde() {
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 42 },
            reply_to: reply_endpoint(),
        };

        let json = serde_json::to_string(&envelope).expect("serialize");
        let decoded: RequestEnvelope<PingRequest> =
            serde_json::from_str(&json).expect("deserialize");

        assert_eq!(decoded.request, envelope.request);
        assert_eq!(decoded.reply_to.token, envelope.reply_to.token);
    }

    #[test]
    fn test_request_stream_try_recv_empty() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.try_recv(|_, _| {});

        assert!(result.is_none());
        assert!(stream.is_empty());
    }

    #[test]
    fn test_request_stream_try_recv() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        // Simulate receiving a request
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 123 },
            reply_to: reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        assert!(!stream.is_empty());
        assert_eq!(stream.len(), 1);

        // Receive the request
        let sent_data: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
        let sent_clone = sent_data.clone();

        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.try_recv(move |_endpoint, payload| {
                *sent_clone.borrow_mut() = Some(payload.to_vec());
            });

        assert!(result.is_some());
        let (request, reply) = result.unwrap();
        assert_eq!(request, PingRequest { seq: 123 });

        // Send a reply
        reply.send(PingResponse { seq: 123 });

        // Verify reply was sent
        assert!(sent_data.borrow().is_some());
    }

    #[tokio::test]
    async fn test_request_stream_recv_async() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        // Simulate receiving a request
        let envelope = RequestEnvelope {
            request: PingRequest { seq: 456 },
            reply_to: reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        // Async receive
        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.recv(|_, _| {}).await;

        assert!(result.is_some());
        let (request, _reply) = result.unwrap();
        assert_eq!(request, PingRequest { seq: 456 });
    }

    #[tokio::test]
    async fn test_request_stream_closed() {
        let stream: RequestStream<PingRequest, JsonCodec> =
            RequestStream::new(test_endpoint(), JsonCodec);

        assert!(!stream.is_closed());
        stream.close();
        assert!(stream.is_closed());

        let result: Option<(PingRequest, ReplyPromise<PingResponse, JsonCodec>)> =
            stream.recv(|_, _| {}).await;

        assert!(result.is_none());
    }
}
