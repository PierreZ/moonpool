//! Simulation workloads for moonpool-transport.
//!
//! Exercises Peer connections, RPC delivery modes, `FailureMonitor`, and
//! `NetTransport` under chaos to find real bugs through deterministic simulation.

pub mod invariants;
pub mod process;
pub mod report;
pub mod service;
pub mod workload;
