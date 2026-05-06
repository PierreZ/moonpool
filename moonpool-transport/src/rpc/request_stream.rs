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
//! // Server loop — transport is bound at construction, no parameters needed.
//! loop {
//!     if let Some((request, reply)) = stream.recv().await {
//!         reply.send(PingResponse { pong: request.ping });
//!     }
//! }
//! ```

use std::marker::PhantomData;
use std::rc::Rc;

use crate::Endpoint;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::net_notified_queue::NetNotifiedQueue;
use super::reply_promise::ReplyPromise;
use super::transport_handle::{EncodeFn, TransportHandle, make_encode_fn};

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
/// comes with a `ReplyPromise` that must be fulfilled. The transport is bound
/// at construction, so `recv()` requires no parameters.
pub struct RequestStream<Req, Resp> {
    /// Queue for incoming request envelopes.
    queue: Rc<NetNotifiedQueue<RequestEnvelope<Req>>>,

    /// Transport for sending replies.
    transport: Rc<dyn TransportHandle>,

    /// Type-erased encode function for replies.
    encode_reply: EncodeFn<Result<Resp, super::reply_error::ReplyError>>,

    _phantom: PhantomData<Resp>,
}

impl<Req, Resp> RequestStream<Req, Resp>
where
    Req: DeserializeOwned + 'static,
    Resp: Serialize + 'static,
{
    /// Create a new request stream with transport bound for replies.
    pub fn new<C: crate::MessageCodec>(
        endpoint: Endpoint,
        codec: C,
        transport: Rc<dyn TransportHandle>,
    ) -> Self {
        Self {
            queue: Rc::new(NetNotifiedQueue::with_codec(endpoint, codec.clone())),
            transport,
            encode_reply: make_encode_fn(codec),
            _phantom: PhantomData,
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
    /// This is used to register the stream with NetTransport.
    pub fn queue(&self) -> Rc<NetNotifiedQueue<RequestEnvelope<Req>>> {
        self.queue.clone()
    }

    /// Receive the next request with its reply promise.
    ///
    /// Returns `None` if the stream is closed. The reply is sent via the
    /// transport bound at construction.
    pub async fn recv(&self) -> Option<(Req, ReplyPromise<Resp>)> {
        let envelope = self.queue.recv().await?;
        let transport_clone = Rc::clone(&self.transport);
        let reply = ReplyPromise::new(
            envelope.reply_to,
            self.encode_reply.clone(),
            move |endpoint, payload| {
                let _ = transport_clone.send_reliable(endpoint, payload);
            },
        );
        Some((envelope.request, reply))
    }

    /// Try to receive a request without blocking.
    ///
    /// Returns `None` if no request is available.
    pub fn try_recv(&self) -> Option<(Req, ReplyPromise<Resp>)> {
        let envelope = self.queue.try_recv()?;
        let transport_clone = Rc::clone(&self.transport);
        let reply = ReplyPromise::new(
            envelope.reply_to,
            self.encode_reply.clone(),
            move |endpoint, payload| {
                let _ = transport_clone.send_reliable(endpoint, payload);
            },
        );
        Some((envelope.request, reply))
    }

    /// Receive the next request using a custom sender function.
    ///
    /// For testing or advanced use cases where replies should bypass the transport.
    pub async fn recv_with_sender<F>(&self, sender: F) -> Option<(Req, ReplyPromise<Resp>)>
    where
        F: FnOnce(&Endpoint, &[u8]) + 'static,
    {
        let envelope = self.queue.recv().await?;
        let reply = ReplyPromise::new(envelope.reply_to, self.encode_reply.clone(), sender);
        Some((envelope.request, reply))
    }

    /// Try to receive a request without blocking, using a custom sender function.
    pub fn try_recv_with_sender<F>(&self, sender: F) -> Option<(Req, ReplyPromise<Resp>)>
    where
        F: FnOnce(&Endpoint, &[u8]) + 'static,
    {
        let envelope = self.queue.try_recv()?;
        let reply = ReplyPromise::new(envelope.reply_to, self.encode_reply.clone(), sender);
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

    use crate::{JsonCodec, NetworkAddress, UID};
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::MessageReceiver;
    use crate::rpc::test_support::make_transport;
    use crate::rpc::transport_handle::TransportHandle;

    fn test_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500);
        Endpoint::new(addr, UID::new(1, 1))
    }

    fn reply_endpoint() -> Endpoint {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4501);
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

    fn make_handle() -> Rc<dyn TransportHandle> {
        let transport = make_transport();
        transport as Rc<dyn TransportHandle>
    }

    fn local_reply_endpoint() -> Endpoint {
        let addr = test_endpoint().address;
        Endpoint::new(addr, UID::new(0xFF, 0xFF))
    }

    #[test]
    fn test_request_stream_endpoint() {
        let handle = make_handle();
        let endpoint = test_endpoint();
        let stream: RequestStream<PingRequest, PingResponse> =
            RequestStream::new(endpoint.clone(), JsonCodec, handle);

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
        let handle = make_handle();
        let stream: RequestStream<PingRequest, PingResponse> =
            RequestStream::new(test_endpoint(), JsonCodec, handle);

        let result = stream.try_recv();
        assert!(result.is_none());
        assert!(stream.is_empty());
    }

    #[test]
    fn test_request_stream_try_recv() {
        let handle = make_handle();
        let stream: RequestStream<PingRequest, PingResponse> =
            RequestStream::new(test_endpoint(), JsonCodec, handle);

        let envelope = RequestEnvelope {
            request: PingRequest { seq: 123 },
            reply_to: reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        assert!(!stream.is_empty());
        assert_eq!(stream.len(), 1);

        let sent_data: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
        let sent_clone = sent_data.clone();

        let result = stream.try_recv_with_sender(move |_endpoint, payload| {
            *sent_clone.borrow_mut() = Some(payload.to_vec());
        });

        assert!(result.is_some());
        let (request, reply): (PingRequest, ReplyPromise<PingResponse>) =
            result.expect("should receive request");
        assert_eq!(request, PingRequest { seq: 123 });

        reply.send(PingResponse { seq: 123 });
        assert!(sent_data.borrow().is_some());
    }

    #[tokio::test]
    async fn test_request_stream_recv_async() {
        let handle = make_handle();
        let stream: RequestStream<PingRequest, PingResponse> =
            RequestStream::new(test_endpoint(), JsonCodec, handle);

        let envelope = RequestEnvelope {
            request: PingRequest { seq: 456 },
            reply_to: reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        let result = stream.recv_with_sender(|_, _| {}).await;
        assert!(result.is_some());
        let (request, _reply): (PingRequest, ReplyPromise<PingResponse>) =
            result.expect("should receive request");
        assert_eq!(request, PingRequest { seq: 456 });
    }

    #[tokio::test]
    async fn test_request_stream_closed() {
        let handle = make_handle();
        let stream: RequestStream<PingRequest, PingResponse> =
            RequestStream::new(test_endpoint(), JsonCodec, handle);

        assert!(!stream.is_closed());
        stream.close();
        assert!(stream.is_closed());

        let result = stream.recv_with_sender(|_, _| {}).await;
        assert!(result.is_none());
    }

    #[test]
    fn test_try_recv_empty() {
        let handle = make_handle();

        let token = UID::new(0xABCD, 0x1234);
        let stream: RequestStream<PingRequest, PingResponse> = RequestStream::new(
            Endpoint::new(test_endpoint().address.clone(), token),
            JsonCodec,
            handle,
        );

        let result = stream.try_recv();
        assert!(result.is_none());
    }

    #[test]
    fn test_try_recv_with_transport() {
        let transport = make_transport();

        let reply_token = local_reply_endpoint().token;
        let reply_queue: Rc<crate::NetNotifiedQueue<Result<PingResponse, crate::ReplyError>>> =
            Rc::new(crate::NetNotifiedQueue::with_codec(
                local_reply_endpoint(),
                JsonCodec,
            ));
        transport.register(
            reply_token,
            Rc::clone(&reply_queue) as Rc<dyn crate::MessageReceiver>,
        );

        let token = UID::new(0xABCD, 0x1234);
        let handle: Rc<dyn TransportHandle> = transport.clone() as Rc<dyn TransportHandle>;
        let stream: RequestStream<PingRequest, PingResponse> = RequestStream::new(
            Endpoint::new(test_endpoint().address.clone(), token),
            JsonCodec,
            handle,
        );

        transport.register(token, stream.queue() as Rc<dyn crate::MessageReceiver>);

        let envelope = RequestEnvelope {
            request: PingRequest { seq: 999 },
            reply_to: local_reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        let result = stream.try_recv();
        assert!(result.is_some());
        let (request, _reply): (PingRequest, ReplyPromise<PingResponse>) =
            result.expect("should receive request");
        assert_eq!(request.seq, 999);
    }

    #[tokio::test]
    async fn test_recv_with_transport() {
        let transport = make_transport();

        let reply_token = local_reply_endpoint().token;
        let reply_queue: Rc<crate::NetNotifiedQueue<Result<PingResponse, crate::ReplyError>>> =
            Rc::new(crate::NetNotifiedQueue::with_codec(
                local_reply_endpoint(),
                JsonCodec,
            ));
        transport.register(
            reply_token,
            Rc::clone(&reply_queue) as Rc<dyn crate::MessageReceiver>,
        );

        let token = UID::new(0xABCD, 0x1234);
        let handle: Rc<dyn TransportHandle> = transport.clone() as Rc<dyn TransportHandle>;
        let stream: RequestStream<PingRequest, PingResponse> = RequestStream::new(
            Endpoint::new(test_endpoint().address.clone(), token),
            JsonCodec,
            handle,
        );

        let envelope = RequestEnvelope {
            request: PingRequest { seq: 888 },
            reply_to: local_reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        let result = stream.recv().await;
        assert!(result.is_some());
        let (request, _reply): (PingRequest, ReplyPromise<PingResponse>) =
            result.expect("should receive request");
        assert_eq!(request.seq, 888);
    }

    #[test]
    fn test_recv_reply_sends() {
        let transport = make_transport();

        let reply_token = local_reply_endpoint().token;
        let reply_queue: Rc<crate::NetNotifiedQueue<Result<PingResponse, crate::ReplyError>>> =
            Rc::new(crate::NetNotifiedQueue::with_codec(
                local_reply_endpoint(),
                JsonCodec,
            ));
        transport.register(
            reply_token,
            Rc::clone(&reply_queue) as Rc<dyn crate::MessageReceiver>,
        );

        let token = UID::new(0xABCD, 0x1234);
        let handle: Rc<dyn TransportHandle> = transport.clone() as Rc<dyn TransportHandle>;
        let stream: RequestStream<PingRequest, PingResponse> = RequestStream::new(
            Endpoint::new(test_endpoint().address.clone(), token),
            JsonCodec,
            handle,
        );

        transport.register(token, stream.queue() as Rc<dyn crate::MessageReceiver>);

        let envelope = RequestEnvelope {
            request: PingRequest { seq: 777 },
            reply_to: local_reply_endpoint(),
        };
        let payload = serde_json::to_vec(&envelope).expect("serialize");
        stream.queue().receive(&payload);

        let result = stream.try_recv();
        let (request, reply): (PingRequest, ReplyPromise<PingResponse>) =
            result.expect("should receive request");
        assert_eq!(request.seq, 777);

        reply.send(PingResponse { seq: 777 });

        let received = reply_queue.try_recv();
        assert!(received.is_some());
        let response = received.expect("should receive response");
        assert_eq!(
            response.expect("response should be Ok"),
            PingResponse { seq: 777 }
        );
    }
}
