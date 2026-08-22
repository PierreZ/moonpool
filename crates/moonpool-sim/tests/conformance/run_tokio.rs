//! Tokio runner: executes each generic contract against `TokioProviders` on a
//! real current-thread runtime. A sim runner that calls the *same* contract
//! bodies against `SimProviders` is the planned follow-up.

use crate::contract_bundle::bundle_contract;
use crate::contract_network::network_contract;
use crate::contract_random::random_contract;
use crate::contract_storage::storage_contract;
use crate::contract_task::task_contract;
use crate::contract_time::time_contract;
use crate::fixtures::TokioFixtures;
use moonpool_core::TokioProviders;

/// Build a current-thread runtime with IO and time enabled (the production shape).
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("failed to build local runtime")
}

#[test]
fn tokio_time_contract() {
    rt().block_on(time_contract(&TokioProviders::new()));
}

#[test]
fn tokio_task_contract() {
    rt().block_on(task_contract(&TokioProviders::new()));
}

#[test]
fn tokio_random_contract() {
    // Synchronous; no runtime needed.
    random_contract(&TokioProviders::new());
}

#[test]
fn tokio_network_contract() {
    rt().block_on(network_contract(
        &TokioProviders::new(),
        &TokioFixtures::new(),
    ));
}

#[test]
fn tokio_storage_contract() {
    rt().block_on(storage_contract(
        &TokioProviders::new(),
        &TokioFixtures::new(),
    ));
}

#[test]
fn tokio_bundle_contract() {
    rt().block_on(bundle_contract(
        &TokioProviders::new(),
        &TokioFixtures::new(),
    ));
}
