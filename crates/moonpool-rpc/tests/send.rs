//! The runtime is written against the local provider traits
//! ([`RpcProviders`](moonpool_rpc::RpcProviders)), so nothing in its
//! signatures promises `Send`. Over any `Send` bundle it must still be
//! `Send`, and so must every future its async methods return, or a Tokio
//! service (or a simulation process, generic over `Providers`) could not
//! spawn them. These assertions are checked when this file compiles, in a
//! context generic over `P: Providers`, as application code is; the tests
//! only run them.

#![cfg(feature = "prost")]

use std::time::Duration;

use moonpool_core::{Providers, TokioProviders};
use moonpool_rpc::{
    FailureMonitor, MethodId, Plaintext, RpcConfig, RpcDriver, RpcHandle, RpcMethod, SchemaVersion,
    ServiceClient,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Ping {
    #[prost(uint64, tag = "1")]
    id: u64,
}

struct Echo;
impl RpcMethod for Echo {
    type Request = Ping;
    type Reply = Ping;
    const METHOD: MethodId = MethodId::new(0xEC40);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "echo";
}

fn is_send<T: Send>(_: &T) {}

const fn send_sync<T: Send + Sync>() {}

/// Build each future over a generic `Send` bundle, and drop it unpolled.
fn futures_are_send<P: Providers>(
    providers: P,
    driver: RpcDriver<P>,
    rpc: &RpcHandle<P>,
    client: &ServiceClient<P, Echo>,
    monitor: &FailureMonitor<P>,
) {
    send_sync::<RpcHandle<P>>();
    send_sync::<ServiceClient<P, Echo>>();
    send_sync::<FailureMonitor<P>>();
    is_send(&driver);
    is_send(&RpcDriver::<P>::listen(
        providers.clone(),
        "127.0.0.1:0",
        RpcConfig::default(),
    ));
    is_send(&RpcDriver::<P, Plaintext>::listen_with(
        providers,
        "127.0.0.1:0",
        RpcConfig::default(),
        Plaintext,
    ));
    is_send(&driver.run());
    is_send(&rpc.shutdown(Duration::from_secs(1)));
    let request = Ping { id: 1 };
    is_send(&client.get_reply(&request));
    is_send(&client.try_get_reply(&request));
    is_send(&client.try_get_reply_within(&request, Duration::from_secs(1)));
    is_send(&client.get_reply_unless_failed_for(&request, Duration::from_secs(1), 1.0));
    if let Ok(attempt) = client.attempt(&request) {
        is_send(&attempt.within(Duration::from_secs(1)));
    }
    is_send(&monitor.on_change());
    let address = "127.0.0.1:1".parse().expect("address");
    is_send(&monitor.on_disconnect(address));
    let endpoint = client.target().endpoint();
    is_send(&monitor.on_disconnect_or_failure(endpoint));
    is_send(&monitor.on_failed_for(endpoint, Duration::from_secs(1), 1.0));
}

#[test]
fn a_runtime_over_a_send_bundle_is_send() {
    let _ = futures_are_send::<TokioProviders>;
}

/// A concrete session upgrade keeps the driver `Send` too: the upgrade
/// traits promise no `Send` future, but a concrete type's futures are.
#[cfg(feature = "tls")]
fn tls_driver_is_send<P: Providers>(driver: RpcDriver<P, moonpool_rpc::security::tls::Tls>) {
    is_send(&driver.run());
}

#[cfg(feature = "tls")]
#[test]
fn a_tls_runtime_over_a_send_bundle_is_send() {
    let _ = tls_driver_is_send::<TokioProviders>;
}
