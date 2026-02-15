//! Simulation test infrastructure for moonpool-paxos.
//!
//! Tests the Vertical Paxos II protocol under deterministic chaos conditions:
//! - Network delays, connection failures, bit flips
//! - Clock drift, partial writes
//! - Leader crashes and reconfiguration
//!
//! ## Test Architecture
//!
//! ```text
//! ┌───────────┐  ┌───────────┐  ┌───────────┐
//! │ Acceptor 1│  │ Acceptor 2│  │ Acceptor 3│   ← 3 acceptor workloads
//! └─────┬─────┘  └─────┬─────┘  └─────┬─────┘
//!       │              │              │
//!       └──────────────┼──────────────┘
//!                      │
//! ┌────────────────────┼────────────────────┐
//! │            Leader / Primary              │   ← leader workload (one of the acceptors)
//! └────────────────────┬────────────────────┘
//!                      │
//!              ┌───────┴───────┐
//!              │    Client     │                  ← client workload
//!              └───────────────┘
//! ```
//!
//! The configuration master is shared in-memory across all workloads (since it's
//! an external service in VP II, not a cluster participant).

#![allow(dead_code)]

pub mod invariants;
#[cfg(test)]
pub mod test_scenarios;
pub mod workloads;
