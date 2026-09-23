//! The generated interface next to the same interface written by hand:
//! identical ids and bytes, interoperable in both directions, every
//! delivery mode and reply policy preserved, the same lifetimes.

#![cfg(feature = "prost")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moonpool_core::TokioProviders;
use moonpool_rpc::{
    AccessClass, Endpoint, EndpointToken, ErrorReason, Execution, Incarnation, IncomingRequest,
    InterfaceId, InterfaceMethod, InterfaceRef, MethodId, RpcConfig, RpcDriver, RpcHandle,
    RpcInterface, RpcMethod, SchemaVersion,
};

#[derive(Clone, PartialEq, prost::Message)]
pub struct Key {
    #[prost(string, tag = "1")]
    pub key: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Value {
    #[prost(string, tag = "1")]
    pub value: String,
}

/// The derived interface.
#[moonpool_rpc_derive::service(id = 0x6b76, version = 3)]
pub trait Kv {
    /// Read a key.
    #[method(id = 10, schema = 2)]
    async fn get(&self, request: Key) -> Value;
    /// Record a key; answers nothing interesting.
    #[method(id = 11, schema = 1)]
    async fn touch(&self, request: Key);
}

/// The same interface by hand.
struct ManualKv;
impl RpcInterface for ManualKv {
    const INTERFACE: InterfaceId = InterfaceId::new(0x6b76);
    const VERSION: SchemaVersion = SchemaVersion::new(3);
    const NAME: &'static str = "manual-kv";
}
struct ManualGet;
impl RpcMethod for ManualGet {
    type Request = Key;
    type Reply = Value;
    const METHOD: MethodId = MethodId::new(10);
    const SCHEMA: SchemaVersion = SchemaVersion::new(2);
    const NAME: &'static str = "manual-kv.get";
}
impl InterfaceMethod<ManualKv> for ManualGet {}
struct ManualTouch;
impl RpcMethod for ManualTouch {
    type Request = Key;
    type Reply = ();
    const METHOD: MethodId = MethodId::new(11);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "manual-kv.touch";
}
impl InterfaceMethod<ManualKv> for ManualTouch {}

/// A handler that counts executions.
#[derive(Default)]
struct Store {
    gets: AtomicU64,
    touches: AtomicU64,
}

impl Kv for Store {
    async fn get(&self, request: Key) -> Value {
        self.gets.fetch_add(1, Ordering::SeqCst);
        Value {
            value: format!("v:{}", request.key),
        }
    }

    async fn touch(&self, _request: Key) {
        self.touches.fetch_add(1, Ordering::SeqCst);
    }
}

fn key(key: &str) -> Key {
    Key { key: key.into() }
}

async fn listen() -> (RpcHandle<TokioProviders>, tokio::task::JoinHandle<()>) {
    let (driver, rpc) =
        RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", RpcConfig::default())
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

#[test]
fn generated_ids_and_bytes_equal_the_manual_ones() {
    assert_eq!(KvInterface::INTERFACE, ManualKv::INTERFACE);
    assert_eq!(KvInterface::VERSION, ManualKv::VERSION);
    assert_eq!(KvGet::METHOD, ManualGet::METHOD);
    assert_eq!(KvGet::SCHEMA, ManualGet::SCHEMA);
    assert_eq!(KvTouch::METHOD, ManualTouch::METHOD);
    assert_eq!(KvTouch::SCHEMA, ManualTouch::SCHEMA);
    let endpoint = Endpoint::new(
        "10.0.0.1:4500".parse().expect("address"),
        Incarnation::from_raw(7),
        EndpointToken::from_parts(1, 2),
    );
    let derived = KvRef::new(endpoint, AccessClass::Public);
    let manual = InterfaceRef::<ManualKv>::new(endpoint, AccessClass::Public);
    assert_eq!(derived.to_bytes(), manual.to_bytes());
    assert_eq!(
        derived.method::<KvGet>().expect("adjusts").to_bytes(),
        manual.method::<ManualGet>().expect("adjusts").to_bytes()
    );
}

/// A derived server answers a manual client, a manual server answers a
/// derived client, with the same outcomes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn derived_and_manual_sides_interoperate() {
    let (server, server_driver) = listen().await;
    let (client, client_driver) = client_only();
    let store = Arc::new(Store::default());

    // Derived server, manual client.
    let derived = KvServer::register(&server, AccessClass::Public).expect("register");
    let published = derived.interface_ref().to_bytes();
    let serving = Arc::clone(&store);
    let derived_task = tokio::spawn(async move { derived.serve(serving.as_ref()).await });
    let manual_view = InterfaceRef::<ManualKv>::from_bytes(&published)
        .expect("same bytes")
        .bind(&client)
        .expect("valid");
    let reply = manual_view
        .method::<ManualGet>()
        .try_get_reply(&key("a"))
        .await;
    assert_eq!(reply.expect("reply").value, "v:a");

    // Manual server, derived client.
    let group = server
        .register_group::<ManualKv>(AccessClass::Public)
        .expect("register");
    let mut get = group.serve::<ManualGet>().expect("serve");
    let manual_task = tokio::spawn(async move {
        while let Some(IncomingRequest { request, reply }) = get.recv().await {
            let _ = reply.send(&Value {
                value: format!("m:{}", request.key),
            });
        }
    });
    let derived_view = KvClient::bind(
        &KvRef::from_bytes(&group.interface_ref().to_bytes()).expect("same bytes"),
        &client,
    )
    .expect("valid");
    let reply = derived_view.get().try_get_reply(&key("b")).await;
    assert_eq!(reply.expect("reply").value, "m:b");
    // A method the manual group does not serve is refused the same way.
    let touch = derived_view.touch().try_get_reply(&key("c")).await;
    assert!(matches!(
        touch
            .as_ref()
            .map_err(|error| (error.reason().clone(), error.execution())),
        Err((ErrorReason::MethodNotFound { .. }, Execution::NotAdmitted))
    ));
    assert_eq!(store.gets.load(Ordering::SeqCst), 1);
    derived_task.abort();
    manual_task.abort();
    server_driver.abort();
    client_driver.abort();
}

/// Every delivery mode goes through the generated client unchanged; the
/// generated dispatcher runs one-way requests and never answers them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_clients_keep_every_delivery_mode() {
    let (server, server_driver) = listen().await;
    let (client, client_driver) = client_only();
    let store = Arc::new(Store::default());
    let kv = KvServer::register(&server, AccessClass::Public).expect("register");
    let target = kv.interface_ref();
    let serving = Arc::clone(&store);
    let task = tokio::spawn(async move { kv.serve(serving.as_ref()).await });
    let client_kv = KvClient::bind(&target, &client).expect("valid");

    client_kv.touch().send(&key("one-way")).expect("queued");
    let reliable = client_kv.get().get_reply(&key("r")).await;
    assert_eq!(reliable.expect("reply").value, "v:r");
    let attempt = client_kv.get().attempt(&key("a")).expect("start");
    assert_eq!(attempt.await.expect("reply").value, "v:a");
    let bounded = client_kv
        .get()
        .get_reply_unless_failed_for(&key("b"), Duration::from_secs(5), 0.0)
        .await;
    assert_eq!(bounded.expect("reply").value, "v:b");
    for _ in 0..500 {
        if store.touches.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(store.touches.load(Ordering::SeqCst), 1);
    let stats = server.stats().expect("running");
    assert_eq!(
        stats.broken_promises, 0,
        "a one-way request is not a broken promise"
    );
    task.abort();
    server_driver.abort();
    client_driver.abort();
}

/// The dispatcher is a convenience, not a policy: a server can pull
/// `KvRequest`s itself and use the reply handles directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_servers_leave_reply_policy_to_the_application() {
    let (server, server_driver) = listen().await;
    let (client, client_driver) = client_only();
    let mut kv = KvServer::register(&server, AccessClass::Public).expect("register");
    let client_kv = KvClient::bind(&kv.interface_ref(), &client).expect("valid");

    let pending = tokio::spawn({
        let client_kv = client_kv.clone();
        async move {
            client_kv
                .get()
                .try_get_reply_within(&key("x"), Duration::from_millis(200))
                .await
        }
    });
    match kv.next().await.expect("a request") {
        KvRequest::Get(IncomingRequest { reply, .. }) => {
            assert!(reply.expects_reply());
            assert!(reply.peer().is_some(), "remote caller");
            reply.never_reply();
        }
        KvRequest::Touch(_) => panic!("unexpected touch"),
    }
    let outcome = pending.await.expect("joined");
    assert!(matches!(
        outcome
            .as_ref()
            .map_err(|error| (error.reason().clone(), error.execution())),
        Err((ErrorReason::Timeout, Execution::MaybeExecuted))
    ));

    // Dropping the generated server drops its group: new requests are
    // refused and the dispatcher's pull ends.
    let target = kv.interface_ref();
    drop(kv);
    let refused = KvClient::bind(&target, &client)
        .expect("valid")
        .get()
        .try_get_reply(&key("y"))
        .await;
    assert!(matches!(
        refused
            .as_ref()
            .map_err(|error| (error.reason().clone(), error.execution())),
        Err((ErrorReason::EndpointNotFound, Execution::NotAdmitted))
    ));
    server_driver.abort();
    client_driver.abort();
}

/// The pull is fair: with both methods loaded, neither starves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_generated_pull_alternates_between_methods() {
    let (server, server_driver) = listen().await;
    let (client, client_driver) = client_only();
    let mut kv = KvServer::register(&server, AccessClass::Public).expect("register");
    let client_kv = KvClient::bind(&kv.interface_ref(), &client).expect("valid");
    for index in 0..4 {
        client_kv
            .get()
            .send(&key(&format!("g{index}")))
            .expect("queued");
        client_kv
            .touch()
            .send(&key(&format!("t{index}")))
            .expect("queued");
    }
    // Wait until all eight are queued at the server.
    for _ in 0..500 {
        if server.stats().expect("running").requests_admitted >= 8 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut order = Vec::new();
    for _ in 0..8 {
        order.push(match kv.next().await.expect("request") {
            KvRequest::Get(_) => 'g',
            KvRequest::Touch(_) => 't',
        });
    }
    assert_eq!(order.iter().collect::<String>(), "gtgtgtgt");
    server_driver.abort();
    client_driver.abort();
}
