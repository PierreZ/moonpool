//! Client-side request sending utilities.
//!
//! Provides the `send_request` function for sending typed requests and
//! receiving typed responses.
//!
//! # Example
//!
//! ```rust,ignore
//! use moonpool::{send_request, ReplyFuture, make_encode_fn, make_decode_fn, JsonCodec};
//!
//! let encode = make_encode_fn::<RequestEnvelope<PingRequest>, _>(JsonCodec);
//! let decode = make_decode_fn::<Result<PingResponse, ReplyError>, _>(JsonCodec);
//! let future: ReplyFuture<PingResponse> = send_request(
//!     &*transport,
//!     &server_endpoint,
//!     PingRequest { seq: 1 },
//!     &encode,
//!     decode,
//! )?;
//!
//! // Await the response
//! let response = future.await?;
//! ```

use std::rc::Rc;

use crate::Endpoint;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::net_notified_queue::{NetNotifiedQueue, ReplyQueueCloser};
use super::reply_error::ReplyError;
use super::reply_future::ReplyFuture;
use super::request_stream::RequestEnvelope;
use super::transport_handle::{DecodeFn, EncodeFn, TransportHandle};
use crate::error::MessagingError;

/// Send a typed request and return a future for the response.
///
/// This is the primary client-side RPC function using **reliable delivery**
/// (at-least-once). It:
/// 1. Creates a temporary endpoint for receiving the reply
/// 2. Wraps the request in a `RequestEnvelope` with the reply endpoint
/// 3. Sends the envelope reliably (survives reconnection)
/// 4. Returns a `ReplyFuture` that resolves when the server responds
///
/// For delivery mode functions with different guarantees, see
/// [`get_reply`](super::delivery::get_reply),
/// [`try_get_reply`](super::delivery::try_get_reply), and
/// [`send`](super::delivery::send).
///
/// # Errors
///
/// Returns `MessagingError` if the request cannot be sent.
pub fn send_request<Req, Resp>(
    transport: &dyn TransportHandle,
    destination: &Endpoint,
    request: Req,
    encode_envelope: &EncodeFn<RequestEnvelope<Req>>,
    decode_reply: DecodeFn<Result<Resp, ReplyError>>,
) -> Result<ReplyFuture<Resp>, MessagingError>
where
    Req: Serialize + 'static,
    Resp: DeserializeOwned + 'static,
{
    prepare_and_send(
        transport,
        destination,
        request,
        encode_envelope,
        decode_reply,
        |t, ep, payload| t.send_reliable(ep, payload),
    )
}

/// Send a typed request **unreliably** and return a future for the response.
///
/// Same as [`send_request`] but uses unreliable transport — the request is
/// dropped on connection failure instead of being retransmitted. Used by
/// [`try_get_reply`](super::delivery::try_get_reply) for at-most-once semantics.
///
/// # FDB Reference
/// `sendUnreliable` in `tryGetReply` (fdbrpc.h:797)
pub(crate) fn send_request_unreliable<Req, Resp>(
    transport: &dyn TransportHandle,
    destination: &Endpoint,
    request: Req,
    encode_envelope: &EncodeFn<RequestEnvelope<Req>>,
    decode_reply: DecodeFn<Result<Resp, ReplyError>>,
) -> Result<ReplyFuture<Resp>, MessagingError>
where
    Req: Serialize + 'static,
    Resp: DeserializeOwned + 'static,
{
    prepare_and_send(
        transport,
        destination,
        request,
        encode_envelope,
        decode_reply,
        |t, ep, payload| t.send_unreliable(ep, payload),
    )
}

/// Shared request preparation: create reply queue, serialize envelope, send via
/// caller-provided function, register pending reply for disconnect cleanup.
fn prepare_and_send<Req, Resp, F>(
    transport: &dyn TransportHandle,
    destination: &Endpoint,
    request: Req,
    encode_envelope: &EncodeFn<RequestEnvelope<Req>>,
    decode_reply: DecodeFn<Result<Resp, ReplyError>>,
    send_fn: F,
) -> Result<ReplyFuture<Resp>, MessagingError>
where
    Req: Serialize + 'static,
    Resp: DeserializeOwned + 'static,
    F: FnOnce(&dyn TransportHandle, &Endpoint, &[u8]) -> Result<(), MessagingError>,
{
    let reply_token = transport.random_uid();

    let reply_endpoint = Endpoint::new(transport.local_address().clone(), reply_token);

    let reply_queue: Rc<NetNotifiedQueue<Result<Resp, ReplyError>>> =
        Rc::new(NetNotifiedQueue::new(reply_endpoint.clone(), decode_reply));

    transport.register(reply_token, reply_queue.clone());

    let envelope = RequestEnvelope {
        request,
        reply_to: reply_endpoint.clone(),
    };

    let payload =
        (encode_envelope)(&envelope).map_err(|e| MessagingError::SerializationFailed {
            message: e.to_string(),
        })?;

    send_fn(transport, destination, &payload)?;

    // Track this reply queue for disconnect cleanup (skip local sends)
    if !transport.is_local_address(&destination.address) {
        let closer: Rc<dyn ReplyQueueCloser> = reply_queue.clone();
        transport.register_pending_reply(
            &destination.address.to_string(),
            reply_token,
            Rc::downgrade(&closer),
        );
    }

    // Attach cleanup callback to unregister the reply endpoint on drop.
    let weak_transport = transport.weak_for_cleanup();
    let future = ReplyFuture::new(reply_queue, reply_endpoint).with_drop_cleanup(move || {
        if let Some(t) = weak_transport.upgrade() {
            t.unregister(&reply_token);
        }
    });

    Ok(future)
}

#[cfg(test)]
mod tests {

    use std::net::{IpAddr, Ipv4Addr};
    use std::rc::Rc;

    use crate::rpc::test_support::make_transport;
    use crate::rpc::transport_handle::{make_decode_fn, make_encode_fn};
    use crate::{JsonCodec, NetworkAddress, UID};
    use serde::{Deserialize, Serialize};

    use super::*;

    fn test_address() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500)
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
    fn test_send_request_creates_future() {
        let transport = make_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<PingRequest>>> = Rc::new(
            NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
        );

        transport.register(server_token, server_queue.clone());

        let encode_envelope = make_encode_fn::<RequestEnvelope<PingRequest>, _>(JsonCodec);
        let decode_reply = make_decode_fn::<Result<PingResponse, ReplyError>, _>(JsonCodec);

        let result = send_request(
            &*transport,
            &server_endpoint,
            PingRequest { seq: 42 },
            &encode_envelope,
            decode_reply,
        );

        assert!(result.is_ok());
        let _future: ReplyFuture<PingResponse> = result.expect("should create future");

        assert!(transport.endpoint_count() >= 2);
        assert_eq!(server_queue.len(), 1);

        let envelope = server_queue.try_recv().expect("should receive envelope");
        assert_eq!(envelope.request, PingRequest { seq: 42 });
        assert!(envelope.reply_to.token.is_valid());
    }

    #[tokio::test]
    async fn test_send_request_roundtrip() {
        let transport = make_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<PingRequest>>> = Rc::new(
            NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
        );

        transport.register(server_token, server_queue.clone());

        let encode_envelope = make_encode_fn::<RequestEnvelope<PingRequest>, _>(JsonCodec);
        let decode_reply = make_decode_fn::<Result<PingResponse, ReplyError>, _>(JsonCodec);

        let future: ReplyFuture<PingResponse> = send_request(
            &*transport,
            &server_endpoint,
            PingRequest { seq: 99 },
            &encode_envelope,
            decode_reply,
        )
        .expect("send_request should succeed");

        let envelope = server_queue.try_recv().expect("should receive request");
        assert_eq!(envelope.request, PingRequest { seq: 99 });

        let response: Result<PingResponse, ReplyError> = Ok(PingResponse { seq: 99 });
        let response_payload = serde_json::to_vec(&response).expect("serialize response");

        transport
            .dispatch(&envelope.reply_to.token, &response_payload)
            .expect("dispatch should succeed");

        let result = future.await;
        assert_eq!(result, Ok(PingResponse { seq: 99 }));
    }
}
