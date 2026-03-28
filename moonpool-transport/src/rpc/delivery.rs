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
use super::net_transport::NetTransport;
use super::reply_error::ReplyError;
use super::reply_future::ReplyFuture;
use super::request::{send_request, send_request_unreliable};
use super::request_stream::RequestEnvelope;
use crate::error::MessagingError;
use crate::{Endpoint, MessageCodec, Providers, TimeProvider, UID};
use moonpool_sim::{assert_reachable, assert_sometimes};

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
pub fn send<Req, P, C>(
    transport: &NetTransport<P>,
    destination: &Endpoint,
    request: Req,
    codec: C,
) -> Result<(), MessagingError>
where
    Req: Serialize,
    P: Providers,
    C: MessageCodec,
{
    // Use a dummy reply endpoint — no queue registered, response silently dropped
    let reply_endpoint = Endpoint::new(transport.local_address().clone(), UID::default());

    let envelope = RequestEnvelope {
        request,
        reply_to: reply_endpoint,
    };

    let payload = codec
        .encode(&envelope)
        .map_err(|e| MessagingError::SerializationFailed {
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
pub async fn try_get_reply<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    destination: &Endpoint,
    request: Req,
    codec: C,
) -> Result<Resp, ReplyError>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec,
{
    let fm = transport.failure_monitor();

    // Fast path: already failed → MaybeDelivered immediately
    if fm.state(destination) == FailureStatus::Failed {
        assert_sometimes!(true, "try_get_reply_fast_path_already_failed");
        return Err(ReplyError::MaybeDelivered);
    }

    let reply_future =
        send_request_unreliable(transport, destination, request, codec).map_err(|e| {
            ReplyError::Serialization {
                message: e.to_string(),
            }
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
pub fn get_reply<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    destination: &Endpoint,
    request: Req,
    codec: C,
) -> Result<ReplyFuture<Resp, C>, MessagingError>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec,
{
    send_request(transport, destination, request, codec)
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
pub async fn get_reply_unless_failed_for<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    destination: &Endpoint,
    request: Req,
    codec: C,
    sustained_failure_duration: Duration,
) -> Result<Resp, ReplyError>
where
    Req: Serialize,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec,
{
    let fm = transport.failure_monitor();
    let time = transport.providers().time().clone();

    let reply_future = send_request(transport, destination, request, codec).map_err(|e| {
        ReplyError::Serialization {
            message: e.to_string(),
        }
    })?;

    let failed_for = on_failed_for(&fm, destination, sustained_failure_duration, &time);
    tokio::pin!(failed_for);

    tokio::select! {
        result = reply_future => match result {
            Ok(resp) => {
                assert_sometimes!(true, "get_reply_unless_failed_for_reply_wins_race");
                Ok(resp)
            }
            Err(ReplyError::BrokenPromise) => {
                fm.endpoint_not_found(destination);
                Err(ReplyError::MaybeDelivered)
            }
            Err(e) => Err(e),
        },
        () = &mut failed_for => {
            assert_sometimes!(true, "get_reply_unless_failed_for_timeout_wins_race");
            Err(ReplyError::MaybeDelivered)
        }
    }
}

/// Wait until endpoint has been failed for at least `duration`.
///
/// First waits for the disconnect/failure signal, then sleeps for
/// the sustained duration. If the connection recovers during the sleep,
/// the reliable retransmit will resolve the reply future first.
async fn on_failed_for<T: TimeProvider>(
    fm: &std::rc::Rc<super::failure_monitor::FailureMonitor>,
    endpoint: &Endpoint,
    duration: Duration,
    time: &T,
) {
    fm.on_disconnect_or_failure(endpoint).await;
    assert_reachable!("on_failed_for_disconnect_observed");
    let _ = time.sleep(duration).await;
    assert_reachable!("on_failed_for_sleep_completed");
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
    use crate::{
        JsonCodec, NetworkAddress, NetworkProvider, Providers, TokioRandomProvider,
        TokioStorageProvider, TokioTaskProvider, TokioTimeProvider, UID,
    };

    // ---- Test infrastructure (shared with request.rs tests) ----

    #[derive(Clone)]
    struct MockNetworkProvider;

    struct DummyStream;

    impl tokio::io::AsyncRead for DummyStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }
    }

    impl tokio::io::AsyncWrite for DummyStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("dummy")))
        }
    }

    impl std::marker::Unpin for DummyStream {}

    struct DummyListener;

    #[async_trait::async_trait(?Send)]
    impl crate::TcpListenerTrait for DummyListener {
        type TcpStream = DummyStream;

        async fn accept(&self) -> std::io::Result<(Self::TcpStream, String)> {
            Err(std::io::Error::other("dummy"))
        }

        fn local_addr(&self) -> std::io::Result<String> {
            Err(std::io::Error::other("dummy"))
        }
    }

    #[async_trait::async_trait(?Send)]
    impl NetworkProvider for MockNetworkProvider {
        type TcpStream = DummyStream;
        type TcpListener = DummyListener;

        async fn bind(&self, _addr: &str) -> std::io::Result<Self::TcpListener> {
            Err(std::io::Error::other("mock"))
        }

        async fn connect(&self, _addr: &str) -> std::io::Result<Self::TcpStream> {
            Err(std::io::Error::other("mock"))
        }
    }

    #[derive(Clone)]
    struct MockProviders {
        network: MockNetworkProvider,
        time: TokioTimeProvider,
        task: TokioTaskProvider,
        random: TokioRandomProvider,
        storage: TokioStorageProvider,
    }

    impl MockProviders {
        fn new() -> Self {
            Self {
                network: MockNetworkProvider,
                time: TokioTimeProvider::new(),
                task: TokioTaskProvider,
                random: TokioRandomProvider::new(),
                storage: TokioStorageProvider::new(),
            }
        }
    }

    impl Providers for MockProviders {
        type Network = MockNetworkProvider;
        type Time = TokioTimeProvider;
        type Task = TokioTaskProvider;
        type Random = TokioRandomProvider;
        type Storage = TokioStorageProvider;

        fn network(&self) -> &Self::Network {
            &self.network
        }
        fn time(&self) -> &Self::Time {
            &self.time
        }
        fn task(&self) -> &Self::Task {
            &self.task
        }
        fn random(&self) -> &Self::Random {
            &self.random
        }
        fn storage(&self) -> &Self::Storage {
            &self.storage
        }
    }

    fn test_address() -> NetworkAddress {
        NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500)
    }

    fn create_test_transport() -> NetTransport<MockProviders> {
        NetTransport::new(test_address(), MockProviders::new())
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestRequest {
        value: u32,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestResponse {
        value: u32,
    }

    // ---- send() tests ----

    #[test]
    fn test_send_fire_and_forget() {
        let transport = create_test_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
        transport.register(server_token, server_queue.clone());

        send(
            &transport,
            &server_endpoint,
            TestRequest { value: 42 },
            JsonCodec,
        )
        .expect("send should succeed");

        // Server received the envelope
        let envelope = server_queue.try_recv().expect("should receive envelope");
        assert_eq!(envelope.request, TestRequest { value: 42 });
        // Reply endpoint uses UID::default (dummy — no listener)
        assert_eq!(envelope.reply_to.token, UID::default());
    }

    // ---- try_get_reply() tests ----

    #[tokio::test]
    async fn test_try_get_reply_already_failed() {
        let transport = create_test_transport();
        let server_endpoint = Endpoint::new(test_address(), UID::new(0x1234, 0x5678));

        // Address unknown → Failed by default → immediate MaybeDelivered
        let result = try_get_reply::<_, TestResponse, _, _>(
            &transport,
            &server_endpoint,
            TestRequest { value: 1 },
            JsonCodec,
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
            let transport = Rc::new(create_test_transport());

            let server_token = UID::new(0x1234, 0x5678);
            let server_endpoint = Endpoint::new(test_address(), server_token);

            let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
                Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
            transport.register(server_token, server_queue.clone());

            transport
                .failure_monitor()
                .set_status("10.0.0.1:4500", FailureStatus::Available);

            let t = Rc::clone(&transport);
            let ep = server_endpoint.clone();
            let handle = tokio::task::spawn_local(async move {
                try_get_reply::<_, TestResponse, _, _>(
                    &t,
                    &ep,
                    TestRequest { value: 99 },
                    JsonCodec,
                )
                .await
            });

            tokio::task::yield_now().await;

            // Server responds
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
            let transport = Rc::new(create_test_transport());

            let server_token = UID::new(0x1234, 0x5678);
            let server_endpoint = Endpoint::new(test_address(), server_token);

            let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
                Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
            transport.register(server_token, server_queue.clone());

            transport
                .failure_monitor()
                .set_status("10.0.0.1:4500", FailureStatus::Available);

            let t = Rc::clone(&transport);
            let ep = server_endpoint.clone();
            let handle = tokio::task::spawn_local(async move {
                try_get_reply::<_, TestResponse, _, _>(&t, &ep, TestRequest { value: 1 }, JsonCodec)
                    .await
            });

            // Yield to let the future register its waker
            tokio::task::yield_now().await;

            // Trigger disconnect → should resolve with MaybeDelivered
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
        let transport = create_test_transport();

        let server_token = UID::new(0x1234, 0x5678);
        let server_endpoint = Endpoint::new(test_address(), server_token);

        let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
            Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
        transport.register(server_token, server_queue.clone());

        let _future: ReplyFuture<TestResponse, JsonCodec> = get_reply(
            &transport,
            &server_endpoint,
            TestRequest { value: 7 },
            JsonCodec,
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
            let transport = Rc::new(create_test_transport());

            let server_token = UID::new(0x1234, 0x5678);
            let server_endpoint = Endpoint::new(test_address(), server_token);

            let server_queue: Rc<NetNotifiedQueue<RequestEnvelope<TestRequest>, JsonCodec>> =
                Rc::new(NetNotifiedQueue::new(server_endpoint.clone(), JsonCodec));
            transport.register(server_token, server_queue.clone());

            transport
                .failure_monitor()
                .set_status("10.0.0.1:4500", FailureStatus::Available);

            let t = Rc::clone(&transport);
            let ep = server_endpoint.clone();
            let handle = tokio::task::spawn_local(async move {
                get_reply_unless_failed_for::<_, TestResponse, _, _>(
                    &t,
                    &ep,
                    TestRequest { value: 55 },
                    JsonCodec,
                    Duration::from_secs(5),
                )
                .await
            });

            tokio::task::yield_now().await;

            // Server responds before timeout
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
