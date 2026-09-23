//! Endpoint groups, checked method adjustment, group lifetimes, independent
//! instances, forwardable callbacks and same-address restarts, on real TCP
//! with the production providers.

#![cfg(feature = "prost")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, IncomingRequest, InterfaceId, InterfaceMethod,
    InterfaceRef, MethodId, RequestStream, RpcConfig, RpcDriver, RpcError, RpcHandle, RpcInterface,
    RpcMethod, SchemaVersion, ServiceRef, WellKnownId, WellKnownRef,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Text {
    #[prost(string, tag = "1")]
    text: String,
}

/// A two-method interface.
struct Kv;
impl RpcInterface for Kv {
    const INTERFACE: InterfaceId = InterfaceId::new(0x4b56);
    const VERSION: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "kv";
}

macro_rules! method {
    ($name:ident, $id:expr, $label:expr) => {
        struct $name;
        impl RpcMethod for $name {
            type Request = Text;
            type Reply = Text;
            const METHOD: MethodId = MethodId::new($id);
            const SCHEMA: SchemaVersion = SchemaVersion::new(1);
            const NAME: &'static str = $label;
        }
        impl InterfaceMethod<Kv> for $name {}
    };
}

method!(Get, 1, "kv.get");
method!(Put, 2, "kv.put");
// A member the server never serves.
method!(Scan, 3, "kv.scan");

fn text(text: &str) -> Text {
    Text { text: text.into() }
}

async fn listen_at(address: &str) -> (RpcHandle<TokioProviders>, tokio::task::JoinHandle<()>) {
    let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), address, RpcConfig::default())
        .await
        .expect("bind");
    (
        rpc,
        tokio::spawn(async move {
            let _ = driver.run().await;
        }),
    )
}

fn client_only() -> (RpcHandle<TokioProviders>, tokio::task::JoinHandle<()>) {
    let (driver, rpc) =
        RpcDriver::client_only(TokioProviders::new(), RpcConfig::default()).expect("config");
    (
        rpc,
        tokio::spawn(async move {
            let _ = driver.run().await;
        }),
    )
}

/// Answer every request with `prefix:text`, counting executions.
fn serve<M>(
    mut stream: RequestStream<M>,
    prefix: &'static str,
    executions: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()>
where
    M: RpcMethod<Request = Text, Reply = Text>,
{
    tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = stream.recv().await {
            executions.fetch_add(1, Ordering::SeqCst);
            let _ = reply.send(&text(&format!("{prefix}:{}", request.text)));
        }
    })
}

fn reason<T>(outcome: &Result<T, RpcError>) -> Option<(ErrorReason, Execution)> {
    outcome
        .as_ref()
        .err()
        .map(|error| (error.reason().clone(), error.execution()))
}

/// One slot serves several methods told apart by their explicit ids; an
/// unserved member is refused before any handler runs; dropping one
/// method's stream removes only that method.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_serves_methods_by_explicit_id() {
    let (server, server_driver) = listen_at("127.0.0.1:0").await;
    let (client, client_driver) = client_only();
    let group = server
        .register_group::<Kv>(AccessClass::Public)
        .expect("register group");
    let executions = Arc::new(AtomicU64::new(0));
    let _get = serve(
        group.serve::<Get>().expect("serve get"),
        "get",
        executions.clone(),
    );
    let put = serve(
        group.serve::<Put>().expect("serve put"),
        "put",
        executions.clone(),
    );
    assert!(matches!(
        group.serve::<Get>().map(|_| ()),
        Err(error) if *error.reason() == ErrorReason::AlreadyRegistered
    ));
    assert_eq!(server.stats().expect("running").endpoints, 1, "one slot");

    // The reference travels as bytes and is bound on the client.
    let kv = InterfaceRef::<Kv>::from_bytes(&group.interface_ref().to_bytes())
        .expect("decodes")
        .bind(&client)
        .expect("valid");
    let got = kv.method::<Get>().try_get_reply(&text("a")).await;
    assert_eq!(got.expect("get").text, "get:a");
    let put_reply = kv.method::<Put>().try_get_reply(&text("b")).await;
    assert_eq!(put_reply.expect("put").text, "put:b");
    let scan = kv.method::<Scan>().try_get_reply(&text("c")).await;
    assert_eq!(
        reason(&scan),
        Some((
            ErrorReason::MethodNotFound {
                called: Scan::METHOD
            },
            Execution::NotAdmitted
        ))
    );
    assert_eq!(executions.load(Ordering::SeqCst), 2);

    // Dropping one stream removes one method; the group keeps serving.
    put.abort();
    let _ = put.await;
    let gone = kv.method::<Put>().try_get_reply(&text("d")).await;
    assert!(matches!(
        reason(&gone),
        Some((ErrorReason::MethodNotFound { .. }, Execution::NotAdmitted))
    ));
    let still = kv.method::<Get>().try_get_reply(&text("e")).await;
    assert_eq!(still.expect("get").text, "get:e");
    // The method can be served again under the same reference.
    let _again = serve(
        group.serve::<Put>().expect("serve again"),
        "put2",
        executions,
    );
    let back = kv.method::<Put>().try_get_reply(&text("f")).await;
    assert_eq!(back.expect("put").text, "put2:f");
    server_driver.abort();
    client_driver.abort();
}

/// Dropping a group rejects new admissions at once, completes queued but
/// unreceived requests as broken promises, and lets a request already
/// received finish under its reply handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_group_rejects_new_admissions_and_admitted_work_finishes() {
    let (server, server_driver) = listen_at("127.0.0.1:0").await;
    let (client, client_driver) = client_only();
    let group = server
        .register_group::<Kv>(AccessClass::Public)
        .expect("register group");
    let mut get = group.serve::<Get>().expect("serve");
    let kv = group.interface_ref().bind(&client).expect("valid");

    let first = kv
        .method::<Get>()
        .attempt(&text("admitted"))
        .expect("start");
    let admitted = tokio::time::timeout(Duration::from_secs(5), get.recv())
        .await
        .expect("arrives")
        .expect("open");
    let second = kv.method::<Get>().attempt(&text("queued")).expect("start");
    // Wait until the second request is queued at the server.
    for _ in 0..500 {
        if server.stats().expect("running").requests_admitted >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(group);
    assert!(get.recv().await.is_none(), "the group's streams end");

    let refused = kv.method::<Get>().try_get_reply(&text("late")).await;
    assert_eq!(
        reason(&refused),
        Some((ErrorReason::EndpointNotFound, Execution::NotAdmitted))
    );
    assert_eq!(
        reason(&second.await),
        Some((ErrorReason::BrokenPromise, Execution::MaybeExecuted))
    );
    // The owned guard of admitted work: its reply still reaches the caller.
    assert!(admitted.reply.send(&text("finished")));
    assert_eq!(first.await.expect("reply").text, "finished");
    server_driver.abort();
    client_driver.abort();
}

/// Two instances of one interface in one runtime stay independent, over
/// the local route and over TCP alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instances_are_independent_locally_and_remotely() {
    let (server, server_driver) = listen_at("127.0.0.1:0").await;
    let (client, client_driver) = client_only();
    let first = server
        .register_group::<Kv>(AccessClass::Public)
        .expect("first");
    let second = server
        .register_group::<Kv>(AccessClass::Public)
        .expect("second");
    let (one, two) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let _a = serve(first.serve::<Get>().expect("serve"), "one", one.clone());
    let _b = serve(second.serve::<Get>().expect("serve"), "two", two.clone());
    assert_ne!(first.endpoint(), second.endpoint());
    for rpc in [&server, &client] {
        let a = first.interface_ref().bind(rpc).expect("valid");
        let b = second.interface_ref().bind(rpc).expect("valid");
        let reply_a = a.method::<Get>().try_get_reply(&text("x")).await;
        let reply_b = b.method::<Get>().try_get_reply(&text("y")).await;
        assert_eq!(reply_a.expect("a").text, "one:x");
        assert_eq!(reply_b.expect("b").text, "two:y");
    }
    assert_eq!(
        (one.load(Ordering::SeqCst), two.load(Ordering::SeqCst)),
        (2, 2)
    );
    // Destroying one instance leaves the other serving.
    drop(first);
    let b = second.interface_ref().bind(&client).expect("valid");
    assert_eq!(
        b.method::<Get>()
            .try_get_reply(&text("z"))
            .await
            .expect("b")
            .text,
        "two:z"
    );
    server_driver.abort();
    client_driver.abort();
}

/// A callback is an ordinary registered endpoint whose reference travels in
/// a request; a third runtime can invoke it, and once dropped it is
/// refused, never redirected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callbacks_are_ordinary_endpoints_passed_through_a_third_runtime() {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Forward {
        #[prost(message, optional, tag = "1")]
        callback: Option<ServiceRef<Get>>,
        #[prost(string, tag = "2")]
        text: String,
    }
    struct Relay;
    impl RpcMethod for Relay {
        type Request = Forward;
        type Reply = Text;
        const METHOD: MethodId = MethodId::new(0x7e1a);
        const SCHEMA: SchemaVersion = SchemaVersion::new(1);
        const NAME: &'static str = "relay";
    }

    // The caller registers the callback; the relay (a third runtime) calls
    // it with whatever it was asked to forward.
    let (caller, caller_driver) = listen_at("127.0.0.1:0").await;
    let (relay, relay_driver) = listen_at("127.0.0.1:0").await;
    let (callback_ref, callback) = caller
        .register::<Get>(AccessClass::Public)
        .expect("callback");
    let calls = Arc::new(AtomicU64::new(0));
    let callback = serve(callback, "cb", calls.clone());
    let (relay_ref, mut relay_stream) =
        relay.register::<Relay>(AccessClass::Public).expect("relay");
    let relay_rpc = relay.clone();
    let relay_task = tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = relay_stream.recv().await {
            let outcome = match request.callback {
                Some(callback) => callback
                    .bind(&relay_rpc)
                    .try_get_reply(&text(&request.text))
                    .await
                    .map_or_else(|error| format!("error:{:?}", error.reason()), |t| t.text),
                None => "no callback".into(),
            };
            let _ = reply.send(&text(&outcome));
        }
    });

    let forward = |text: &str| Forward {
        callback: Some(callback_ref.clone()),
        text: text.into(),
    };
    let relayed = relay_ref.bind(&caller).try_get_reply(&forward("hi")).await;
    assert_eq!(relayed.expect("relayed").text, "cb:hi");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The callback is withdrawn: the stale reference is refused.
    callback.abort();
    let _ = callback.await;
    let stale = relay_ref
        .bind(&caller)
        .try_get_reply(&forward("again"))
        .await;
    assert_eq!(stale.expect("relayed").text, "error:EndpointNotFound");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    relay_task.abort();
    caller_driver.abort();
    relay_driver.abort();
}

/// A participant restarts at the same address: its old interface never
/// dispatches to the new incarnation, and callers use the new one only
/// after learning it through the participant's well-known directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_at_the_same_address_is_learned_only_through_republication() {
    restart_scenario().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_restart_at_the_same_address_is_learned_only_through_republication_current_thread() {
    restart_scenario().await;
}

async fn restart_scenario() {
    struct Directory;
    impl RpcMethod for Directory {
        type Request = Text;
        type Reply = InterfaceRef<Kv>;
        const METHOD: MethodId = MethodId::new(0xd1);
        const SCHEMA: SchemaVersion = SchemaVersion::new(1);
        const NAME: &'static str = "directory";
    }
    const DIRECTORY: WellKnownId = WellKnownId::new(1);

    /// One boot of the participant: a Kv group plus the directory that
    /// publishes it.
    async fn boot(
        address: &str,
        name: &'static str,
        executions: Arc<AtomicU64>,
    ) -> Option<(RpcHandle<TokioProviders>, Vec<tokio::task::JoinHandle<()>>)> {
        let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), address, RpcConfig::default())
            .await
            .ok()?;
        let group = rpc.register_group::<Kv>(AccessClass::Public).ok()?;
        let get = serve(group.serve::<Get>().ok()?, name, executions);
        let (_, mut directory) = rpc
            .register_well_known::<Directory>(DIRECTORY, AccessClass::Public)
            .ok()?;
        let published = group.interface_ref();
        let directory = tokio::spawn(async move {
            let _group = group;
            while let Some(IncomingRequest { reply, .. }) = directory.recv().await {
                let _ = reply.send(&published);
            }
        });
        let driver = tokio::spawn(async move {
            let _ = driver.run().await;
        });
        Some((rpc, vec![driver, get, directory]))
    }

    let (first_calls, second_calls) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (a, first_boot) = boot("127.0.0.1:0", "i1", first_calls.clone())
        .await
        .expect("first boot");
    let address = a.address().expect("listening");
    let (b, b_driver) = client_only();
    let directory = WellKnownRef::<Directory>::new(
        moonpool_rpc::BootstrapAddress::Resolved(address),
        DIRECTORY,
        AccessClass::Public,
    )
    .at(address)
    .bind(&b);

    let i1 = directory.try_get_reply(&text("")).await.expect("learn I1");
    let i1_client = i1.bind(&b).expect("valid");
    let reply = i1_client.method::<Get>().try_get_reply(&text("x")).await;
    assert_eq!(reply.expect("I1 serves").text, "i1:x");

    // Crash A (drop its driver and everything it owned) and boot it again
    // at the same address.
    for task in first_boot {
        task.abort();
        let _ = task.await;
    }
    let Some((_a2, _second_boot)) = boot(&address.to_string(), "i2", second_calls.clone()).await
    else {
        eprintln!("could not rebind {address}; skipping");
        b_driver.abort();
        return;
    };

    // I1 is refused, terminally; it never reaches I2's handler, even as a
    // reliable call.
    let stale = i1_client.method::<Get>().get_reply(&text("y")).await;
    assert_eq!(
        reason(&stale),
        Some((ErrorReason::StaleIncarnation, Execution::NotAdmitted))
    );
    assert!(stale.expect_err("stale").is_terminal_for_reference());
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);

    // B learns I2 explicitly and uses it.
    let i2 = directory.try_get_reply(&text("")).await.expect("learn I2");
    assert_eq!(i2.endpoint().address(), address, "same address");
    assert_ne!(i2.endpoint().incarnation(), i1.endpoint().incarnation());
    let reply = i2
        .bind(&b)
        .expect("valid")
        .method::<Get>()
        .try_get_reply(&text("z"))
        .await;
    assert_eq!(reply.expect("I2 serves").text, "i2:z");
    assert_eq!(
        (
            first_calls.load(Ordering::SeqCst),
            second_calls.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
    // I1 still fails, now without a round trip.
    let before = b.stats().expect("running").calls_failed_fast;
    let again = i1_client.method::<Get>().try_get_reply(&text("w")).await;
    assert!(matches!(
        reason(&again),
        Some((ErrorReason::StaleIncarnation, Execution::NotAdmitted))
    ));
    assert!(b.stats().expect("running").calls_failed_fast > before);
    b_driver.abort();
}
