//! Simulation workloads for moonpool-transport.
//!
//! Exercises Peer connections, RPC delivery modes, FailureMonitor, and
//! NetTransport under chaos to find real bugs through deterministic simulation.

/// Echo service types and hand-rolled endpoint constants.
pub mod service;

/// Server process that runs the echo service.
pub mod process;

/// Client workload that drives random operations across all delivery modes.
pub mod workload;

/// Delivery contract invariants checked via timelines.
pub mod invariants;

/// Custom transport-specific simulation report.
pub mod report;
