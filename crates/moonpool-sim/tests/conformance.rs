//! Provider conformance suite.
//!
//! Proves that `TokioProviders` is a faithful drop-in for the raw
//! `tokio::*` / `std` / `rand` calls it replaces, so existing production code
//! can migrate to the provider traits without behavioural regression.
//!
//! Each contract is generic over [`moonpool_core::Providers`] and asserts the
//! invariants a correct implementation must satisfy (not literal outputs). Only
//! the Tokio runner exists today; because the contract bodies are runtime-
//! agnostic, a sim runner that drives the same bodies against `SimProviders`
//! drops in later to turn this into a sim↔tokio parity harness.
//!
//! The whole suite is gated on `tokio-providers`: with the feature off (the
//! wasm/`--no-default-features` build) there is nothing to run.
#![cfg(feature = "tokio-providers")]

#[path = "conformance/fixtures.rs"]
mod fixtures;

#[path = "conformance/contract_bundle.rs"]
mod contract_bundle;
#[path = "conformance/contract_network.rs"]
mod contract_network;
#[path = "conformance/contract_random.rs"]
mod contract_random;
#[path = "conformance/contract_storage.rs"]
mod contract_storage;
#[path = "conformance/contract_task.rs"]
mod contract_task;
#[path = "conformance/contract_time.rs"]
mod contract_time;

#[path = "conformance/run_tokio.rs"]
mod run_tokio;
