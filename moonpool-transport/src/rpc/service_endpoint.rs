//! Client-side typed endpoint for making RPC calls.
//!
//! FDB equivalent: `RequestStream<T>` on the caller side (fdbrpc.h:726-948).
//!
//! A [`ServiceEndpoint`] is a typed wrapper around an [`Endpoint`] that provides
//! delivery mode methods matching FoundationDB's `getReply`, `tryGetReply`,
//! `send`, and `getReplyUnlessFailedFor`.
//!
//! # Example
//!
//! ```rust,ignore
//! let calc = CalculatorClient::from_base(server_addr, base_token, JsonCodec, &transport);
//!
//! // Each delivery mode is a call-site decision:
//! let resp = calc.add.get_reply(req).await?;
//! let resp = calc.add.try_get_reply(req).await?;
//! calc.add.send(req)?;
//! ```

use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::instrument;

use super::delivery;
use super::reply_error::ReplyError;
use super::reply_future::ReplyFuture;
use super::request_stream::RequestEnvelope;
use super::rpc_error::RpcError;
use super::transport_handle::{
    DecodeFn, EncodeFn, TransportHandle, make_decode_fn, make_encode_fn,
};
use crate::Endpoint;
use crate::error::MessagingError;

/// Client-side typed endpoint for making RPC calls.
///
/// Wraps an [`Endpoint`] with request/response type information and
/// a bound transport. The transport and codec are captured at construction
/// so delivery methods require only the request payload.
///
/// # Delivery Modes
///
/// | Method | Guarantee | On Disconnect |
/// |--------|-----------|---------------|
/// | [`send`](Self::send) | Fire-and-forget | Silently lost |
/// | [`try_get_reply`](Self::try_get_reply) | At-most-once | `MaybeDelivered` |
/// | [`get_reply`](Self::get_reply) | At-least-once | Retransmits |
/// | [`get_reply_unless_failed_for`](Self::get_reply_unless_failed_for) | At-least-once + timeout | `MaybeDelivered` after duration |
///
/// # FDB Reference
/// `RequestStream<T>` (fdbrpc.h:726-948)
pub struct ServiceEndpoint<Req, Resp> {
    endpoint: Endpoint,
    transport: Rc<dyn TransportHandle>,
    encode_envelope: EncodeFn<RequestEnvelope<Req>>,
    decode_reply: DecodeFn<Result<Resp, ReplyError>>,
    _phantom: PhantomData<fn(Req) -> Resp>,
}

impl<Req, Resp> std::fmt::Debug for ServiceEndpoint<Req, Resp> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceEndpoint")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl<Req, Resp> Clone for ServiceEndpoint<Req, Resp> {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            transport: Rc::clone(&self.transport),
            encode_envelope: self.encode_envelope.clone(),
            decode_reply: self.decode_reply.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<Req, Resp> serde::Serialize for ServiceEndpoint<Req, Resp> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.endpoint.serialize(serializer)
    }
}

impl<Req, Resp> ServiceEndpoint<Req, Resp> {
    /// Create a new service endpoint from a raw endpoint, codec, and transport.
    pub fn new<C: crate::MessageCodec>(
        endpoint: Endpoint,
        codec: C,
        transport: Rc<dyn TransportHandle>,
    ) -> Self
    where
        Req: Serialize + 'static,
        Resp: DeserializeOwned + 'static,
    {
        Self {
            endpoint,
            transport,
            encode_envelope: make_encode_fn(codec.clone()),
            decode_reply: make_decode_fn(codec),
            _phantom: PhantomData,
        }
    }

    /// Get the underlying endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl<Req, Resp> ServiceEndpoint<Req, Resp>
where
    Req: Serialize + 'static,
    Resp: DeserializeOwned + 'static,
{
    /// Fire-and-forget delivery: send request unreliably with no reply.
    ///
    /// # FDB Reference
    /// `RequestStream::send` (fdbrpc.h:733-738)
    ///
    /// # Errors
    ///
    /// Returns `RpcError::Messaging` if serialization or the send itself fails.
    #[instrument(skip_all)]
    pub fn send(&self, req: Req) -> Result<(), RpcError> {
        delivery::send(&*self.transport, &self.endpoint, req, &self.encode_envelope)
            .map_err(RpcError::from)
    }

    /// At-most-once delivery: send unreliably, race reply against disconnect.
    ///
    /// # FDB Reference
    /// `RequestStream::tryGetReply` (fdbrpc.h:784-826)
    ///
    /// # Errors
    ///
    /// Returns `MaybeDelivered` on disconnect.
    #[instrument(skip_all)]
    pub async fn try_get_reply(&self, req: Req) -> Result<Resp, RpcError> {
        delivery::try_get_reply(
            &*self.transport,
            &self.endpoint,
            req,
            &self.encode_envelope,
            self.decode_reply.clone(),
        )
        .await
        .map_err(RpcError::from)
    }

    /// At-least-once delivery: send reliably, retransmit on reconnect.
    ///
    /// # FDB Reference
    /// `RequestStream::getReply` (fdbrpc.h:752-762)
    ///
    /// # Errors
    ///
    /// Returns `RpcError` if the request cannot be sent or the reply fails.
    #[instrument(skip_all)]
    pub async fn get_reply(&self, req: Req) -> Result<Resp, RpcError> {
        let future = delivery::get_reply(
            &*self.transport,
            &self.endpoint,
            req,
            &self.encode_envelope,
            self.decode_reply.clone(),
        )?;
        Ok(future.await?)
    }

    /// At-least-once delivery with sustained failure timeout.
    ///
    /// # FDB Reference
    /// `RequestStream::getReplyUnlessFailedFor` (fdbrpc.h:870-895)
    ///
    /// # Errors
    ///
    /// Returns `MaybeDelivered` if the endpoint is failed for longer than
    /// `sustained_failure_duration`.
    #[instrument(skip_all)]
    pub async fn get_reply_unless_failed_for(
        &self,
        req: Req,
        sustained_failure_duration: Duration,
    ) -> Result<Resp, RpcError> {
        delivery::get_reply_unless_failed_for(
            &*self.transport,
            &self.endpoint,
            req,
            &self.encode_envelope,
            self.decode_reply.clone(),
            sustained_failure_duration,
        )
        .await
        .map_err(RpcError::from)
    }

    /// Send a typed request and return a raw [`ReplyFuture`] for the response.
    ///
    /// # Errors
    ///
    /// Returns `MessagingError` if the request cannot be sent.
    #[instrument(skip_all)]
    pub fn send_request(&self, req: Req) -> Result<ReplyFuture<Resp>, MessagingError> {
        delivery::get_reply(
            &*self.transport,
            &self.endpoint,
            req,
            &self.encode_envelope,
            self.decode_reply.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::rpc::failure_monitor::FailureStatus;
    use crate::rpc::net_notified_queue::NetNotifiedQueue;
    use crate::rpc::request_stream::RequestEnvelope;
    use crate::rpc::test_support::make_transport;
    use crate::{JsonCodec, MessageCodec, NetworkAddress, UID};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct PingRequest {
        seq: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PingResponse {
        seq: u32,
    }

    #[test]
    fn test_send_fire_and_forget() {
        let transport = make_transport();
        let addr = transport.local_address().clone();
        let token = UID::new(0x1234, 0x0001);
        let endpoint = Endpoint::new(addr, token);

        let handle: Rc<dyn TransportHandle> = transport.clone() as Rc<dyn TransportHandle>;
        let ep: ServiceEndpoint<PingRequest, PingResponse> =
            ServiceEndpoint::new(endpoint, JsonCodec, handle);

        let result = ep.send(PingRequest { seq: 1 });
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_try_get_reply_already_failed() {
        let transport = make_transport();
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5001);
        let token = UID::new(0x5678, 0x0001);
        let endpoint = Endpoint::new(addr.clone(), token);

        transport
            .failure_monitor()
            .set_status(&addr.to_string(), FailureStatus::Failed);

        let handle: Rc<dyn TransportHandle> = transport.clone() as Rc<dyn TransportHandle>;
        let ep: ServiceEndpoint<PingRequest, PingResponse> =
            ServiceEndpoint::new(endpoint, JsonCodec, handle);

        let result = ep.try_get_reply(PingRequest { seq: 2 }).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_reply_creates_future() {
        let transport = make_transport();
        let server_addr = transport.local_address().clone();
        let token = UID::new(0x9ABC, 0x0001);
        let server_endpoint = Endpoint::new(server_addr.clone(), token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<PingRequest>>> = Rc::new(
            NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
        );
        transport.register(
            token,
            Rc::clone(&server_queue) as Rc<dyn crate::MessageReceiver>,
        );

        let handle: Rc<dyn TransportHandle> = transport.clone() as Rc<dyn TransportHandle>;
        let ep: ServiceEndpoint<PingRequest, PingResponse> =
            ServiceEndpoint::new(server_endpoint, JsonCodec, handle);

        let future = ep.send_request(PingRequest { seq: 3 });
        assert!(future.is_ok());

        let received = server_queue.try_recv();
        assert!(received.is_some());
        let envelope = received.expect("request envelope");
        assert_eq!(envelope.request.seq, 3);

        let reply_endpoint = envelope.reply_to;
        let response: Result<PingResponse, ReplyError> = Ok(PingResponse { seq: 3 });
        let payload = JsonCodec.encode(&response).expect("encode");
        transport
            .dispatch(&reply_endpoint.token, &payload)
            .expect("dispatch");

        let result = future.expect("future").await;
        assert_eq!(result.expect("response"), PingResponse { seq: 3 });
    }
}
