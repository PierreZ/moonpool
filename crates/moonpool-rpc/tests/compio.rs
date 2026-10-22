//! The RPC runtime over `CompioProviders`: the same runtime code as the Tokio
//! runs in `tests/real_network.rs` and the simulation campaign, on the local
//! (non-`Send`) provider traits and a thread-per-core compio runtime
//! (`io_uring` on Linux, polling where a seccomp profile or an old kernel
//! refuses it).
//!
//! Run with `cargo test -p moonpool-rpc --features compio-providers`.
//! Each test drives the drivers and the callers on one thread, as a
//! thread-per-core application does.

use std::future::Future;
use std::time::Duration;

use moonpool_core::{CompioProviders, CompioTaskProvider, LocalTaskProvider};
use moonpool_rpc::security::SecurityConfig;
use moonpool_rpc::{
    AccessClass, ErrorReason, IncomingRequest, MethodId, RequestStream, RpcConfig, RpcDriver,
    RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Ping {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(string, tag = "2")]
    text: String,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Ping;
    type Reply = Ping;
    const METHOD: MethodId = MethodId::new(0xEC40);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

fn ping(id: u64) -> Ping {
    Ping {
        id,
        text: format!("ping-{id}"),
    }
}

/// Run `future` to completion on a fresh compio runtime.
fn run<F: Future>(future: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(future)
}

/// A compio task: cancelled when the handle is dropped.
type Task<T> = compio::runtime::JoinHandle<T>;

/// A listening runtime on an ephemeral port, its driver spawned on the
/// current thread. Private endpoints need an explicit trust choice: a
/// trusted network, which claims nothing.
async fn listen() -> (RpcHandle<CompioProviders>, Task<std::io::Error>) {
    let config = RpcConfig {
        security: SecurityConfig::trusted_network(),
        ..RpcConfig::default()
    };
    let (driver, rpc) = RpcDriver::listen(CompioProviders::new(), "127.0.0.1:0", config)
        .await
        .expect("bind an ephemeral port");
    (rpc, compio::runtime::spawn(driver.run()))
}

fn client_only() -> (RpcHandle<CompioProviders>, Task<std::io::Error>) {
    let (driver, rpc) =
        RpcDriver::client_only(CompioProviders::new(), RpcConfig::default()).expect("valid config");
    (rpc, compio::runtime::spawn(driver.run()))
}

/// Answer every request with its own body, on a local task.
fn serve_echo(mut stream: RequestStream<Echo>) {
    // Detached: the handler ends when the runtime drops its endpoint.
    drop(CompioTaskProvider.spawn_local_task("echo", async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            let _ = reply.send(&request);
        }
    }));
}

#[test]
fn two_runtimes_exchange_typed_replies() {
    run(async {
        let (server, _server_driver) = listen().await;
        let (client, _client_driver) = client_only();
        let (service, stream) = server
            .register::<Echo>(AccessClass::Private)
            .expect("register");
        serve_echo(stream);

        // The reference travels as plain bytes, decoded without a runtime.
        let service = ServiceRef::<Echo>::from_bytes(&service.to_bytes()).expect("decodes");
        let echo = service.bind(&client);
        for id in 0..50 {
            let reply = echo.try_get_reply(&ping(id)).await.expect("reply");
            assert_eq!(reply, ping(id));
        }
        // Calls in flight together share the one session.
        let calls = (100..132).map(|id| {
            let echo = echo.clone();
            async move { echo.get_reply(&ping(id)).await }
        });
        for (id, reply) in (100..).zip(futures::future::join_all(calls).await) {
            assert_eq!(reply.expect("reply"), ping(id));
        }
        let stats = client.stats().expect("running");
        assert_eq!(stats.calls_started, 82);
        assert_eq!(stats.pending_calls, 0);
        assert_eq!(stats.connections, 1, "one session is reused");
    });
}

#[test]
fn a_call_nobody_answers_times_out_on_compio_time() {
    run(async {
        let (server, _server_driver) = listen().await;
        let (client, _client_driver) = client_only();
        // Registered, never served: the request waits in the queue.
        let (service, _stream) = server
            .register::<Echo>(AccessClass::Private)
            .expect("register");
        let error = service
            .bind(&client)
            .try_get_reply_within(&ping(1), Duration::from_millis(100))
            .await
            .expect_err("nobody answers");
        assert_eq!(error.reason(), &ErrorReason::Timeout);
    });
}

#[test]
fn a_dropped_server_fails_the_call_and_the_monitor_sees_it() {
    run(async {
        let (server, server_driver) = listen().await;
        let (client, _client_driver) = client_only();
        let (service, stream) = server
            .register::<Echo>(AccessClass::Private)
            .expect("register");
        serve_echo(stream);
        let echo = service.bind(&client);
        echo.try_get_reply(&ping(1)).await.expect("first reply");

        let monitor = client.failure_monitor().expect("running");
        let disconnected = monitor.on_disconnect(service.endpoint().address());
        // Dropping the task cancels the driver: the abrupt shutdown.
        drop(server_driver);
        disconnected.await.expect("the session ends");
        let error = echo
            .try_get_reply_within(&ping(2), Duration::from_secs(5))
            .await
            .expect_err("the server is gone");
        assert_ne!(error.reason(), &ErrorReason::Timeout, "{error}");
    });
}

#[test]
fn a_graceful_shutdown_finishes() {
    run(async {
        let (server, _server_driver) = listen().await;
        let (client, client_driver) = client_only();
        let (service, stream) = server
            .register::<Echo>(AccessClass::Private)
            .expect("register");
        serve_echo(stream);
        service
            .bind(&client)
            .try_get_reply(&ping(1))
            .await
            .expect("reply");
        let report = client.shutdown(Duration::from_secs(1)).await;
        assert!(!report.already_stopped, "{report:?}");
        assert!(client.is_shutting_down());
        drop(client_driver);
    });
}

/// Thread-per-core: two OS threads, each with its own compio runtime,
/// sharing nothing but the reference bytes.
#[test]
fn two_threads_each_with_a_runtime_exchange_replies() {
    let (reference_tx, reference_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (done_tx, done_rx) = futures::channel::oneshot::channel::<()>();
    let server = std::thread::spawn(move || {
        run(async move {
            let (rpc, _driver) = listen().await;
            let (service, stream) = rpc
                .register::<Echo>(AccessClass::Private)
                .expect("register");
            serve_echo(stream);
            reference_tx
                .send(service.to_bytes())
                .expect("send reference");
            done_rx.await.expect("done");
        });
    });
    let bytes = reference_rx.recv().expect("reference");
    run(async move {
        let (rpc, _driver) = client_only();
        let echo = ServiceRef::<Echo>::from_bytes(&bytes)
            .expect("decodes")
            .bind(&rpc);
        for id in 0..20 {
            assert_eq!(
                echo.try_get_reply(&ping(id)).await.expect("reply"),
                ping(id)
            );
        }
    });
    done_tx.send(()).expect("stop server");
    server.join().expect("server thread");
}
