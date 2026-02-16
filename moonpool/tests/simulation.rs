//! Actor simulation test suite.
//!
//! FDB-style alphabet workloads for the virtual actor system:
//! conservation laws, lifecycle verification, and chaos testing.

#[path = "simulation/invariants.rs"]
mod invariants;
#[path = "simulation/operations.rs"]
mod operations;
#[path = "simulation/test_scenarios.rs"]
mod test_scenarios;
#[path = "simulation/workloads.rs"]
mod workloads;
