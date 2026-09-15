//! Provider conformance suite.
//!
//! Proves that provider bundles meet the same contracts. `TokioProviders` is
//! a faithful drop-in for the raw `tokio::*` / `std` / `rand` calls it
//! replaces, while `SimProviders` exercises the network contract on the
//! deterministic executor.
//!
//! Each contract is generic over [`moonpool_core::Providers`] and asserts the
//! invariants a correct implementation must satisfy (not literal outputs). Only
//! the runner supplies the runtime-specific fixtures and driver.

#[path = "conformance/fixtures.rs"]
mod fixtures;

#[cfg(feature = "tokio-providers")]
#[path = "conformance/contract_bundle.rs"]
mod contract_bundle;
#[path = "conformance/contract_network.rs"]
mod contract_network;
#[cfg(feature = "tokio-providers")]
#[path = "conformance/contract_random.rs"]
mod contract_random;
#[cfg(feature = "tokio-providers")]
#[path = "conformance/contract_storage.rs"]
mod contract_storage;
#[cfg(feature = "tokio-providers")]
#[path = "conformance/contract_task.rs"]
mod contract_task;
#[cfg(feature = "tokio-providers")]
#[path = "conformance/contract_time.rs"]
mod contract_time;
#[path = "conformance/run_sim.rs"]
mod run_sim;

#[cfg(feature = "tokio-providers")]
#[path = "conformance/run_tokio.rs"]
mod run_tokio;
