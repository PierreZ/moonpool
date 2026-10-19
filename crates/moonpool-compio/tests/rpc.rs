//! moonpool-rpc over `CompioProviders`: a generated service on loopback,
//! a reply, and a call that times out through the rpc timeout path.

use std::time::Duration;

use moonpool_compio::CompioProviders;
use moonpool_core::{Detach, Providers, TaskProvider};
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, IncomingRequest, RpcConfig, RpcDriver, RpcHandle,
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

/// A two-method service.
#[moonpool_rpc::service(id = 0x6b76, version = 1)]
pub trait Kv {
    /// Read a key.
    #[method(id = 10, schema = 1)]
    async fn get(&self, request: Key) -> Value;
    /// Record a key.
    #[method(id = 11, schema = 1)]
    async fn touch(&self, request: Key);
}

struct Store;

impl Kv for Store {
    async fn get(&self, request: Key) -> Value {
        Value {
            value: format!("v:{}", request.key),
        }
    }

    async fn touch(&self, _request: Key) {}
}

fn key(key: &str) -> Key {
    Key { key: key.into() }
}

/// A listening runtime whose driver runs as a task on this compio thread.
async fn listen(providers: &CompioProviders) -> RpcHandle<CompioProviders> {
    let (driver, rpc) = RpcDriver::listen(providers.clone(), "127.0.0.1:0", RpcConfig::default())
        .await
        .expect("bind");
    providers
        .task()
        .spawn_task("rpc-server", async move {
            let _ = driver.run().await;
        })
        .detach();
    rpc
}

fn client_only(providers: &CompioProviders) -> RpcHandle<CompioProviders> {
    let (driver, rpc) =
        RpcDriver::client_only(providers.clone(), RpcConfig::default()).expect("config");
    providers
        .task()
        .spawn_task("rpc-client", async move {
            let _ = driver.run().await;
        })
        .detach();
    rpc
}

#[compio::test]
async fn a_generated_service_answers_over_compio() {
    let providers = CompioProviders::new();
    let server = listen(&providers).await;
    let client = client_only(&providers);
    let kv = KvServer::register(&server, AccessClass::Public).expect("register");
    let target = kv.interface_ref();
    providers
        .task()
        .spawn_task("kv", async move { kv.serve(&Store).await })
        .detach();

    let client_kv = KvClient::bind(&target, &client).expect("valid");
    let reply = client_kv.get().try_get_reply(&key("a")).await;
    assert_eq!(reply.expect("reply").value, "v:a");
    let reliable = client_kv.get().get_reply(&key("b")).await;
    assert_eq!(reliable.expect("reply").value, "v:b");
}

/// A request the server holds without answering times out on the
/// provider's clock, as maybe executed.
#[compio::test]
async fn an_unanswered_call_times_out() {
    let providers = CompioProviders::new();
    let server = listen(&providers).await;
    let client = client_only(&providers);
    let mut kv = KvServer::register(&server, AccessClass::Public).expect("register");
    let client_kv = KvClient::bind(&kv.interface_ref(), &client).expect("valid");

    let get = client_kv.get();
    let request = key("x");
    let call = get.try_get_reply_within(&request, Duration::from_millis(200));
    let hold = async {
        match kv.next().await.expect("a request") {
            KvRequest::Get(IncomingRequest { reply, .. }) => reply.never_reply(),
            KvRequest::Touch(_) => panic!("unexpected touch"),
        }
    };
    let (outcome, ()) = futures::join!(call, hold);
    assert!(matches!(
        outcome
            .as_ref()
            .map_err(|error| (error.reason().clone(), error.execution())),
        Err((ErrorReason::Timeout, Execution::MaybeExecuted))
    ));
}
