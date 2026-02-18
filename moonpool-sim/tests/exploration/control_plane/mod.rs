//! Control-plane integration test: fork-based exploration finds consistency bugs
//! caused by a flaky message broker.
//!
//! Demonstrates moonpool's RPC over simulated network with 4 workloads:
//! - MessageBroker (P=0.10 bug: operations succeed internally but return errors)
//! - ControlPlane (produces requests, polls responses, maintains VM registry)
//! - Node (polls requests, processes commands, maintains VM set)
//! - Driver (orchestrates the test scenario, detects divergence)

mod broker;
mod cp;
mod driver;
mod node;
mod tests;
