//! FDB-style delivery modes for typed RPC.
//!
//! Four delivery guarantees matching FoundationDB's fdbrpc layer:
//!
//! | Function | Guarantee | Transport | On Disconnect |
//! |----------|-----------|-----------|---------------|
//! | [`send`] | Fire-and-forget | unreliable | Silently lost |
//! | [`try_get_reply`] | At-most-once | unreliable | `MaybeDelivered` |
//! | [`get_reply`] | At-least-once | reliable | Retransmits |
//! | [`get_reply_unless_failed_for`] | At-least-once + timeout | reliable | `MaybeDelivered` after duration |
//!
//! # FDB Reference
//! `RequestStream::send`, `tryGetReply`, `getReply`, `getReplyUnlessFailedFor`
//! from `fdbrpc.h:727-895`

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use super::failure_monitor::FailureStatus;
use super::reply_error::ReplyError;
use super::reply_future::ReplyFuture;
use super::request::{send_request, send_request_unreliable};
use super::request_stream::RequestEnvelope;
use super::transport_handle::{DecodeFn, EncodeFn, TransportHandle};
use crate::error::MessagingError;
use crate::{Endpoint, UID};

/// Fire-and-forget delivery: send request unreliably with no reply.
///
/// The request is sent once via unreliable transport. No reply endpoint is
/// registered, so any server response is silently discarded.
///
/// # FDB Reference
/// `RequestStream::send` (fdbrpc.h:733-738)
///
/// # Errors
///
/// Returns `MessagingError` if serialization or the send itself fails.
pub fn send<Req>(
    transport: &dyn TransportHandle,
    destination: &Endpoint,
    request: Req,
    encode_envelope: &EncodeFn<RequestEnvelope<Req>>,
) -> Result<(), MessagingError>
where
    Req: Serialize + 'static,
{
    let reply_endpoint = Endpoint::new(transport.local_address().clone(), UID::default());

    let envelope = RequestEnvelope {
        request,
        reply_to: reply_endpoint,
    };

    let payload =
        (encode_envelope)(&envelope).map_err(|e| MessagingError::SerializationFailed {
            message: e.to_string(),
        })?;

    transport.send_unreliable(destination, &payload)
}

/// At-most-once delivery: send unreliably, race reply against disconnect.
///
/// Returns `Ok(response)` if the server replies before disconnect,
/// or `Err(ReplyError::MaybeDelivered)` if the connection fails while
/// the request is in flight (the request may or may not have been processed).
///
/// Callers must be prepared to handle ambiguity — typically via
/// read-before-retry (Strategy 4) or idempotent requests.
///
/// # FDB Reference
/// `RequestStream::tryGetReply` (fdbrpc.h:784-826),
/// `waitValueOrSignal` (genericactors.actor.h:362-398)
///
/// # Errors
///
/// Returns `ReplyError::MaybeDelivered` on disconnect or if already failed.
/// Returns other `ReplyError` variants for serialization errors, timeouts, etc.
pub async fn try_get_reply<Req, Resp>(
    transport: &dyn TransportHandle,
    destination: &Endpoint,
    request: Req,
    encode_envelope: &EncodeFn<RequestEnvelope<Req>>,
    decode_reply: DecodeFn<Result<Resp, ReplyError>>,
) -> Result<Resp, ReplyError>
where
    Req: Serialize + 'static,
    Resp: DeserializeOwned + 'static,
{
    let fm = transport.failure_monitor();

    if fm.state(destination) == FailureStatus::Failed {
        return Err(ReplyError::MaybeDelivered);
    }

    let reply_future = send_request_unreliable(
        transport,
        destination,
        request,
        encode_envelope,
        decode_reply,
    )
    .map_err(|e| ReplyError::Serialization {
        message: e.to_string(),
    })?;

    let disconnect = fm.on_disconnect_or_failure(destination);
    tokio::pin!(disconnect);

    tokio::select! {
        result = reply_future => match result {
            Ok(resp) => Ok(resp),
            Err(ReplyError::BrokenPromise) => {
                fm.endpoint_not_found(destination);
                Err(ReplyError::MaybeDelivered)
            }
            Err(e) => Err(e),
        },
        () = &mut disconnect => Err(ReplyError::MaybeDelivered),
    }
}

/// At-least-once delivery: send reliably, retransmit on reconnect.
///
/// Equivalent to [`send_request`] with FDB-aligned naming. The request
/// is queued for reliable delivery and will be retransmitted if the
/// connection drops and reconnects.
///
/// # FDB Reference
/// `RequestStream::getReply` (fdbrpc.h:752-762)
///
/// # Errors
///
/// Returns `MessagingError` if the request cannot be sent.
pub fn get_reply<Req, Resp>(
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
    send_request(
        transport,
        destination,
        request,
        encode_envelope,
        decode_reply,
    )
}

/// At-least-once delivery with sustained failure timeout.
///
/// Like [`get_reply`] but gives up if the endpoint has been continuously
/// failed for `sustained_failure_duration`. Returns `MaybeDelivered` on
/// timeout, allowing the caller to handle ambiguity.
///
/// # FDB Reference
/// `RequestStream::getReplyUnlessFailedFor` (fdbrpc.h:870-895)
///
/// # Errors
///
/// Returns `ReplyError::MaybeDelivered` if the endpoint is failed for
/// longer than `sustained_failure_duration`.
pub async fn get_reply_unless_failed_for<Req, Resp>(
    transport: &dyn TransportHandle,
    destination: &Endpoint,
    request: Req,
    encode_envelope: &EncodeFn<RequestEnvelope<Req>>,
    decode_reply: DecodeFn<Result<Resp, ReplyError>>,
    sustained_failure_duration: Duration,
) -> Result<Resp, ReplyError>
where
    Req: Serialize + 'static,
    Resp: DeserializeOwned + 'static,
{
    let fm = transport.failure_monitor();

    let reply_future = send_request(
        transport,
        destination,
        request,
        encode_envelope,
        decode_reply,
    )
    .map_err(|e| ReplyError::Serialization {
        message: e.to_string(),
    })?;

    let failed_for = on_failed_for(transport, destination, sustained_failure_duration);
    tokio::pin!(failed_for);

    tokio::select! {
        result = reply_future => match result {
            Ok(resp) => Ok(resp),
            Err(ReplyError::BrokenPromise) => {
                fm.endpoint_not_found(destination);
                Err(ReplyError::MaybeDelivered)
            }
            Err(e) => Err(e),
        },
        () = &mut failed_for => {
            Err(ReplyError::MaybeDelivered)
        }
    }
}

/// Wait until endpoint has been failed for at least `duration`.
///
/// First waits for the disconnect/failure signal, then sleeps for
/// the sustained duration. If the connection recovers during the sleep,
/// the reliable retransmit will resolve the reply future first.
async fn on_failed_for(transport: &dyn TransportHandle, endpoint: &Endpoint, duration: Duration) {
    transport
        .failure_monitor()
        .on_disconnect_or_failure(endpoint)
        .await;
    transport.time_sleep(duration).await;
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
    use crate::rpc::transport_handle::{make_decode_fn, make_encode_fn};
    use crate::{JsonCodec, NetworkAddress, UID};

    fn test_address() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500)
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestRequest {
        value: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestResponse {
        value: u32,
    }

    fn test_encode() -> EncodeFn<RequestEnvelope<TestRequest>> {
        make_encode_fn(JsonCodec)
    }

    fn test_decode() -> DecodeFn<Result<TestResponse, ReplyError>> {
        make_decode_fn(JsonCodec)
    }

    // ---- send() tests ----

    #[test]
    fn test_send_fire_and_forget() {
        let transport = make_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>>> = Rc::new(
            NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
        );
        transport.register(server_token, server_queue.clone());

        send(
            &*transport,
            &server_endpoint,
            TestRequest { value: 42 },
            &test_encode(),
        )
        .expect("send should succeed");

        let envelope = server_queue.try_recv().expect("should receive envelope");
        assert_eq!(envelope.request, TestRequest { value: 42 });
        assert_eq!(envelope.reply_to.token, UID::default());
    }

    // ---- try_get_reply() tests ----

    #[tokio::test]
    async fn test_try_get_reply_already_failed() {
        let transport = make_transport();
        let server_endpoint = Endpoint::new(test_address(), UID::new(0x1234, 0x5678));

        let result = try_get_reply(
            &*transport,
            &server_endpoint,
            TestRequest { value: 1 },
            &test_encode(),
            test_decode(),
        )
        .await;

        assert_eq!(result, Err(ReplyError::MaybeDelivered));
    }

    #[test]
    fn test_try_get_reply_success() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");
        rt.block_on(async {
            let transport = make_transport();

            let server_token = UID::new(0x1234, 0x5678);
            let server_endpoint = Endpoint::new(test_address(), server_token);

            let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>>> = Rc::new(
                NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
            );
            transport.register(server_token, server_queue.clone());

            transport
                .failure_monitor()
                .set_status("10.0.0.1:4500", FailureStatus::Available);

            let t = Rc::clone(&transport);
            let encode = test_encode();
            let handle = tokio::task::spawn_local(async move {
                let ep = Endpoint::new(test_address(), UID::new(0x1234, 0x5678));
                try_get_reply(&*t, &ep, TestRequest { value: 99 }, &encode, test_decode()).await
            });

            tokio::task::yield_now().await;

            let envelope = server_queue.try_recv().expect("should receive request");
            let response: Result<TestResponse, ReplyError> = Ok(TestResponse { value: 99 });
            let response_payload = serde_json::to_vec(&response).expect("serialize response");
            transport
                .dispatch(&envelope.reply_to.token, &response_payload)
                .expect("dispatch should succeed");

            let result = handle.await.expect("task should complete");
            assert_eq!(result, Ok(TestResponse { value: 99 }));
        });
    }

    #[test]
    fn test_try_get_reply_disconnect_during_wait() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");
        rt.block_on(async {
            let transport = make_transport();

            let server_token = UID::new(0x1234, 0x5678);
            let server_endpoint = Endpoint::new(test_address(), server_token);

            let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>>> = Rc::new(
                NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
            );
            transport.register(server_token, server_queue.clone());

            transport
                .failure_monitor()
                .set_status("10.0.0.1:4500", FailureStatus::Available);

            let t = Rc::clone(&transport);
            let encode = test_encode();
            let handle = tokio::task::spawn_local(async move {
                let ep = Endpoint::new(test_address(), UID::new(0x1234, 0x5678));
                try_get_reply(&*t, &ep, TestRequest { value: 1 }, &encode, test_decode()).await
            });

            tokio::task::yield_now().await;

            let fm = transport.failure_monitor();
            fm.set_status("10.0.0.1:4500", FailureStatus::Failed);
            fm.notify_disconnect("10.0.0.1:4500");

            let result = handle.await.expect("task should complete");
            assert_eq!(result, Err(ReplyError::MaybeDelivered));
        });
    }

    // ---- get_reply() tests ----

    #[test]
    fn test_get_reply_delegates_to_send_request() {
        let transport = make_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>>> = Rc::new(
            NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
        );
        transport.register(server_token, server_queue.clone());

        let _future: ReplyFuture<TestResponse> = get_reply(
            &*transport,
            &server_endpoint,
            TestRequest { value: 7 },
            &test_encode(),
            test_decode(),
        )
        .expect("get_reply should succeed");

        let envelope = server_queue.try_recv().expect("should receive envelope");
        assert_eq!(envelope.request, TestRequest { value: 7 });
        assert!(envelope.reply_to.token.is_valid());
    }

    // ---- get_reply_unless_failed_for() tests ----

    #[test]
    fn test_get_reply_unless_failed_for_success() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("build runtime");
        rt.block_on(async {
            let transport = make_transport();

            let server_token = UID::new(0x1234, 0x5678);
            let server_endpoint = Endpoint::new(test_address(), server_token);

            let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>>> = Rc::new(
                NetNotifiedQueue::with_codec(server_endpoint.clone(), JsonCodec),
            );
            transport.register(server_token, server_queue.clone());

            transport
                .failure_monitor()
                .set_status("10.0.0.1:4500", FailureStatus::Available);

            let t = Rc::clone(&transport);
            let encode = test_encode();
            let handle = tokio::task::spawn_local(async move {
                let ep = Endpoint::new(test_address(), UID::new(0x1234, 0x5678));
                get_reply_unless_failed_for(
                    &*t,
                    &ep,
                    TestRequest { value: 55 },
                    &encode,
                    test_decode(),
                    Duration::from_secs(5),
                )
                .await
            });

            tokio::task::yield_now().await;

            let envelope = server_queue.try_recv().expect("should receive request");
            let response: Result<TestResponse, ReplyError> = Ok(TestResponse { value: 55 });
            let response_payload = serde_json::to_vec(&response).expect("serialize");
            transport
                .dispatch(&envelope.reply_to.token, &response_payload)
                .expect("dispatch");

            let result = handle.await.expect("task should complete");
            assert_eq!(result, Ok(TestResponse { value: 55 }));
        });
    }
}
