//! Graceful and abrupt shutdown on real TCP (both Tokio flavors): admission
//! closes, admitted work drains within the deadline, what remains ends
//! with honest execution knowledge, and everything is released.

#![cfg(feature = "prost")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::security::SecurityConfig;
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, IncomingRequest, MethodId, RpcConfig, RpcDriver,
    RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Text {
    #[prost(string, tag = "1")]
    text: String,
}

struct Slow;
impl RpcMethod for Slow {
    type Request = Text;
    type Reply = Text;
    const METHOD: MethodId = MethodId::new(0x5d01);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "slow";
}

struct Items;
impl RpcMethod for Items {
    type Request = Text;
    type Reply = Text;
    const METHOD: MethodId = MethodId::new(0x5d02);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "items";
    const STREAMING: bool = true;
}

const CALL: Duration = Duration::from_secs(10);

fn trusted() -> RpcConfig {
    RpcConfig {
        security: SecurityConfig::trusted_network(),
        ..RpcConfig::default()
    }
}

/// A server whose driver runs on its own task and whose handle can shut it
/// down; the driver task ends when the shutdown resolved.
async fn server(
    config: RpcConfig,
) -> (
    RpcHandle<TokioProviders>,
    tokio::task::JoinHandle<std::io::Error>,
) {
    let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", config)
        .await
        .expect("bind");
    (rpc, tokio::spawn(driver.run()))
}

fn client() -> (
    RpcHandle<TokioProviders>,
    tokio::task::JoinHandle<std::io::Error>,
) {
    let (driver, rpc) = RpcDriver::client_only(TokioProviders::new(), trusted()).expect("config");
    (rpc, tokio::spawn(driver.run()))
}

fn text(text: &str) -> Text {
    Text { text: text.into() }
}

fn call(
    rpc: &RpcHandle<TokioProviders>,
    target: &ServiceRef<Slow>,
) -> tokio::task::JoinHandle<Result<Text, moonpool_rpc::RpcError>> {
    let client = target.bind(rpc);
    tokio::spawn(async move { client.try_get_reply_within(&text("hi"), CALL).await })
}

/// Admission closes, the admitted call drains and completes, new work is
/// refused (remote: `ServerShuttingDown`, local: `Shutdown`, both never
/// admitted), and the runtime closes cleanly and is released.
async fn a_graceful_shutdown_drains_admitted_work() {
    let (server, server_driver) = server(trusted()).await;
    let probe = server.probe().expect("running");
    let (service, mut stream) = server.register::<Slow>(AccessClass::Private).expect("reg");
    let executed = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&executed);
    let handler = tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = reply.send(&request);
        }
    });
    let (client, client_driver) = client();
    let in_flight = call(&client, &service);
    while executed.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let shutdown = {
        let server = server.clone();
        tokio::spawn(async move { server.shutdown(Duration::from_secs(5)).await })
    };
    while !server.is_shutting_down() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let refused = call(&client, &service)
        .await
        .expect("join")
        .expect_err("admission closed");
    assert_eq!(refused.execution(), Execution::NotAdmitted);
    assert!(
        matches!(
            refused.reason(),
            ErrorReason::ServerShuttingDown | ErrorReason::ConnectFailed(_)
        ),
        "{refused}"
    );
    let local = service
        .bind(&server)
        .try_get_reply_within(&text("local"), CALL)
        .await
        .expect_err("closed locally too");
    assert_eq!(local.reason(), &ErrorReason::Shutdown);
    assert_eq!(local.execution(), Execution::NotAdmitted);
    assert!(server.register::<Slow>(AccessClass::Public).is_err());

    let reply = in_flight.await.expect("join").expect("drained");
    assert_eq!(reply.text, "hi");
    let report = shutdown.await.expect("join");
    assert!(report.drained, "{report:?}");
    assert!(report.closed_cleanly, "{report:?}");
    assert_eq!(report.calls_ended, 0);
    assert_eq!(report.replies_abandoned, 0);
    assert_eq!(executed.load(Ordering::SeqCst), 1);
    // The driver can now go: everything it owned is released.
    server_driver.abort();
    let _ = server_driver.await;
    handler.abort();
    assert!(probe.is_released(), "tasks {}", probe.live_tasks());
    client_driver.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn a_graceful_shutdown_drains_admitted_work_current_thread() {
    a_graceful_shutdown_drains_admitted_work().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_graceful_shutdown_drains_admitted_work_multi_thread() {
    a_graceful_shutdown_drains_admitted_work().await;
}

/// At the deadline, what remains ends honestly: a request admitted but not
/// answered (and one queued behind it in a full endpoint queue) ends as a
/// disconnect after admission (`MaybeExecuted`), never as "executed" or
/// "not admitted"; a request beyond the full queue was refused before
/// admission.
async fn the_deadline_ends_remaining_work_honestly() {
    let config = RpcConfig {
        endpoint_queue_capacity: 1,
        ..trusted()
    };
    let (server, server_driver) = server(config).await;
    let (service, mut stream) = server.register::<Slow>(AccessClass::Private).expect("reg");
    let received = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&received);
    let handler = tokio::spawn(async move {
        // Takes the first request and holds it, never pulls again: the
        // queue behind it fills.
        let first = stream.recv().await;
        counter.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_hours(1)).await;
        drop((first, stream));
    });
    let (client, client_driver) = client();
    let held = call(&client, &service);
    while received.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let queued = call(&client, &service);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let overflow = call(&client, &service)
        .await
        .expect("join")
        .expect_err("queue full");
    assert_eq!(overflow.reason(), &ErrorReason::Overloaded);
    assert_eq!(overflow.execution(), Execution::NotAdmitted);

    let report = server.shutdown(Duration::from_millis(200)).await;
    assert!(!report.drained);
    assert_eq!(report.replies_abandoned, 2, "{report:?}");
    assert_eq!(report.connections_closed, 1);
    assert!(report.closed_cleanly);
    for outcome in [held, queued] {
        let error = outcome.await.expect("join").expect_err("abandoned");
        assert_eq!(error.reason(), &ErrorReason::Disconnected);
        assert_eq!(error.execution(), Execution::MaybeExecuted);
    }
    server_driver.abort();
    handler.abort();
    client_driver.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn the_deadline_ends_remaining_work_honestly_current_thread() {
    the_deadline_ends_remaining_work_honestly().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deadline_ends_remaining_work_honestly_multi_thread() {
    the_deadline_ends_remaining_work_honestly().await;
}

/// A caller's shutdown ends its own pending work: a retained (reliable)
/// call that was sent is `Shutdown` + `MaybeExecuted` and its retention is
/// released; a call still waiting for a session that never completes its
/// handshake was never sent (`NotAdmitted`).
async fn a_callers_shutdown_ends_its_calls_with_what_it_knows() {
    let (server, server_driver) = server(trusted()).await;
    let (service, mut stream) = server.register::<Slow>(AccessClass::Private).expect("reg");
    let received = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&received);
    let handler = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(request) = stream.recv().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(request);
        }
    });
    // A listener that accepts TCP but never speaks: the handshake hangs.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let silent_address = silent.local_addr().expect("address");
    let mut dead_end = service.clone();
    {
        let endpoint = moonpool_rpc::Endpoint::new(
            silent_address,
            service.endpoint().incarnation(),
            service.endpoint().token(),
        );
        dead_end = ServiceRef::new(endpoint, dead_end.access());
    }
    let (client, client_driver) = client();
    let reliable = {
        let client = service.bind(&client);
        tokio::spawn(async move { client.get_reply(&text("reliable")).await })
    };
    let connecting = call(&client, &dead_end);
    while received.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let before = client.stats().expect("running");
    assert_eq!(before.retained_calls, 1);
    let report = client.shutdown(Duration::from_millis(100)).await;
    assert_eq!(report.calls_ended, 2, "{report:?}");
    assert!(report.closed_cleanly, "{report:?}");
    let reliable = reliable.await.expect("join").expect_err("ended");
    assert_eq!(reliable.reason(), &ErrorReason::Shutdown);
    assert_eq!(reliable.execution(), Execution::MaybeExecuted);
    let connecting = connecting.await.expect("join").expect_err("ended");
    assert_eq!(connecting.reason(), &ErrorReason::Shutdown);
    assert_eq!(connecting.execution(), Execution::NotAdmitted);
    let after = client.stats().expect("still referenced");
    assert_eq!(after.retained_calls, 0);
    assert_eq!(after.pending_calls, 0);
    assert_eq!(after.connections, 0);
    drop(silent);
    client_driver.abort();
    server_driver.abort();
    handler.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn a_callers_shutdown_ends_its_calls_current_thread() {
    a_callers_shutdown_ends_its_calls_with_what_it_knows().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_callers_shutdown_ends_its_calls_multi_thread() {
    a_callers_shutdown_ends_its_calls_with_what_it_knows().await;
}

/// Termination closes every endpoint: a request still queued, never
/// received, is not handed to its handler afterwards (its caller's session
/// ended), and the receiver sees the end of its stream. Regression: the
/// inboxes stayed open and kept handing out requests after termination.
async fn termination_closes_every_endpoint() {
    let (server, server_driver) = server(trusted()).await;
    let (service, mut stream) = server.register::<Slow>(AccessClass::Private).expect("reg");
    let (client, client_driver) = client();
    let queued = call(&client, &service);
    tokio::time::timeout(CALL, async {
        while server
            .stats()
            .is_some_and(|stats| stats.requests_admitted == 0)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("admitted into the queue");
    let report = server.shutdown(Duration::from_millis(50)).await;
    assert!(!report.drained);
    assert!(
        tokio::time::timeout(CALL, stream.recv())
            .await
            .expect("the stream answers")
            .is_none(),
        "no request is handed out after termination"
    );
    let error = queued.await.expect("join").expect_err("ended");
    assert_eq!(error.execution(), Execution::MaybeExecuted, "{error}");
    client_driver.abort();
    server_driver.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn termination_closes_every_endpoint_current_thread() {
    termination_closes_every_endpoint().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn termination_closes_every_endpoint_multi_thread() {
    termination_closes_every_endpoint().await;
}

/// Two shutdowns at once: the first one's deadline governs, the second
/// waits for it and reports the same outcome; nothing is terminated twice.
/// Regression: the second (with a zero grace) terminated at once, cutting
/// the first one's drain short.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_shutdowns_share_one_termination() {
    let (server, server_driver) = server(trusted()).await;
    let (service, stream) = server.register::<Slow>(AccessClass::Private).expect("reg");
    let (client, client_driver) = client();
    let queued = call(&client, &service);
    tokio::time::timeout(CALL, async {
        while server
            .stats()
            .is_some_and(|stats| stats.requests_admitted == 0)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("admitted");
    let first = {
        let server = server.clone();
        tokio::spawn(async move { server.shutdown(Duration::from_millis(800)).await })
    };
    while !server.is_shutting_down() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let started = std::time::Instant::now();
    let second = server.shutdown(Duration::ZERO).await;
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the second shutdown waited for the first one's deadline"
    );
    let first = first.await.expect("join");
    assert_eq!(first, second, "one outcome, reported to both");
    assert_eq!(first.replies_abandoned, 1);
    assert_eq!(
        server
            .stats()
            .expect("still referenced")
            .calls_ended_by_shutdown,
        0
    );
    let _ = queued.await;
    drop(stream);
    client_driver.abort();
    server_driver.abort();
}

/// Streams: a stream that finishes within the grace drains; one still
/// producing at the deadline ends on both sides, after every item that
/// arrived.
async fn active_streams_drain_or_end_at_the_deadline() {
    let (server, server_driver) = server(trusted()).await;
    let (items, mut stream) = server.register::<Items>(AccessClass::Private).expect("reg");
    let handler = tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let Ok(producer) = reply.into_stream() else {
                continue;
            };
            tokio::spawn(async move {
                let endless = request.text == "endless";
                let mut sent = 0u32;
                while endless || sent < 3 {
                    if producer.send(&text("item")).await.is_err() {
                        return;
                    }
                    sent += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let _ = producer.finish();
            });
        }
    });
    let (client, client_driver) = client();
    let mut finite = items
        .bind(&client)
        .get_reply_stream(&text("three"))
        .expect("open");
    let mut endless = items
        .bind(&client)
        .get_reply_stream(&text("endless"))
        .expect("open");
    let first = tokio::time::timeout(CALL, endless.recv())
        .await
        .expect("an item")
        .expect("open")
        .expect("ok");
    assert_eq!(first.text, "item");
    let finite_items = tokio::spawn(async move {
        let mut count = 0;
        while let Some(item) = finite.recv().await {
            item.expect("the finite stream ends normally");
            count += 1;
        }
        count
    });
    let endless_end = tokio::spawn(async move {
        loop {
            match endless.recv().await {
                Some(Ok(_)) => {}
                Some(Err(error)) => return Some(error),
                None => return None,
            }
        }
    });
    let report = server.shutdown(Duration::from_millis(300)).await;
    assert!(!report.drained, "the endless stream cannot drain");
    assert!(report.replies_abandoned >= 1, "{report:?}");
    assert_eq!(finite_items.await.expect("join"), 3);
    let error = endless_end
        .await
        .expect("join")
        .expect("ends with an error");
    // Items arrived, so the handler ran: the stream ends as a disconnect of
    // an executed request.
    assert_eq!(error.reason(), &ErrorReason::Disconnected, "{error}");
    assert_eq!(error.execution(), Execution::Executed, "{error}");
    let stats = client.stats().expect("running");
    assert_eq!(stats.streams_consuming, 0);
    client_driver.abort();
    server_driver.abort();
    handler.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn active_streams_drain_or_end_at_the_deadline_current_thread() {
    active_streams_drain_or_end_at_the_deadline().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_streams_drain_or_end_at_the_deadline_multi_thread() {
    active_streams_drain_or_end_at_the_deadline().await;
}

/// Dropping the driver is the abrupt shutdown: a graceful shutdown started
/// meanwhile resolves at once, and every call fails with `Shutdown`.
#[tokio::test(flavor = "current_thread")]
async fn dropping_the_driver_overtakes_a_graceful_shutdown() {
    let (server, server_driver) = server(trusted()).await;
    let probe = server.probe().expect("running");
    let (service, stream) = server.register::<Slow>(AccessClass::Private).expect("reg");
    // Something is owed, so the drain cannot finish: a local call admitted
    // into a queue nobody reads.
    let waiting = {
        let client = service.bind(&server);
        tokio::spawn(async move { client.try_get_reply(&text("x")).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    let shutdown = {
        let server = server.clone();
        tokio::spawn(async move { server.shutdown(Duration::from_hours(1)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(server.is_shutting_down());
    server_driver.abort();
    let _ = server_driver.await;
    drop(stream);
    let report = shutdown.await.expect("join");
    assert!(report.already_stopped, "{report:?}");
    let error = waiting.await.expect("join").expect_err("gone");
    assert_eq!(error.reason(), &ErrorReason::Shutdown, "{error}");
    assert_ne!(error.execution(), Execution::Executed, "{error}");
    assert!(probe.is_at_baseline(), "{:?}", probe.outstanding());
    assert!(server.shutdown(Duration::ZERO).await.already_stopped);
}

/// Every gauge a runtime accounts (owed replies, produced streams, queued
/// bytes, reserved windows, buffered stream bytes) is held by an owner and
/// returns to zero once that owner is gone, even after the driver was
/// dropped under it: `ResourceProbe::is_at_baseline`.
async fn every_gauge_returns_to_zero_once_its_owners_are_gone() {
    let (server, server_driver) = server(trusted()).await;
    let server_probe = server.probe().expect("running");
    let (slow, mut slow_stream) = server.register::<Slow>(AccessClass::Public).expect("reg");
    let (items, mut items_stream) = server.register::<Items>(AccessClass::Public).expect("reg");
    // Owners the application keeps: a reply handle it never answers, and a
    // producer that sends until its credit runs out.
    let (held_tx, mut held_rx) = tokio::sync::mpsc::unbounded_channel();
    let holder = tokio::spawn(async move {
        while let Some(IncomingRequest { reply, .. }) = slow_stream.recv().await {
            let _ = held_tx.send(reply);
        }
    });
    let producer = tokio::spawn(async move {
        let Some(IncomingRequest { reply, .. }) = items_stream.recv().await else {
            return;
        };
        let Ok(producer) = reply.into_stream() else {
            return;
        };
        for index in 0..10_000 {
            if producer
                .send(&text(&format!("item {index:05}")))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let (client, client_driver) = client();
    let client_probe = client.probe().expect("running");
    let stream = items
        .bind(&client)
        .get_reply_stream_with_window(&text("scan"), 4096)
        .expect("opened");
    let pending = {
        let slow = slow.bind(&client);
        tokio::spawn(async move { slow.try_get_reply_within(&text("held"), CALL).await })
    };
    let held = held_rx.recv().await.expect("the unary call was admitted");
    loop {
        let producing = server.stats().expect("running");
        let consuming = client.stats().expect("running");
        if producing.streams_producing == 1
            && producing.inflight_requests >= 2
            && consuming.stream_buffered_bytes > 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!server_probe.outstanding().is_empty());
    assert!(!client_probe.outstanding().is_empty());

    // Abrupt: both drivers go while every owner is still alive.
    server_driver.abort();
    client_driver.abort();
    let _ = server_driver.await;
    let _ = client_driver.await;
    let error = pending.await.expect("join").expect_err("driver gone");
    assert_ne!(error.execution(), Execution::NotAdmitted, "{error}");
    assert!(
        !server_probe.is_at_baseline(),
        "the held reply handle still accounts for its request: {:?}",
        server_probe.outstanding()
    );

    drop(held);
    producer.abort();
    let _ = producer.await;
    holder.abort();
    let _ = holder.await;
    drop(stream);
    assert!(
        server_probe.is_at_baseline(),
        "{server_probe:?} {:?}",
        server_probe.outstanding()
    );
    assert!(
        client_probe.is_at_baseline(),
        "{client_probe:?} {:?}",
        client_probe.outstanding()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn every_gauge_returns_to_zero_once_its_owners_are_gone_current_thread() {
    every_gauge_returns_to_zero_once_its_owners_are_gone().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_gauge_returns_to_zero_once_its_owners_are_gone_multi_thread() {
    every_gauge_returns_to_zero_once_its_owners_are_gone().await;
}
