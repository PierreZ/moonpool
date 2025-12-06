//! # Moonpool
//!
//! Deterministic simulation testing for distributed systems in Rust.
//!
//! Moonpool enables you to write distributed system logic once, test it with
//! simulated networking for reproducible debugging, then deploy with real
//! networking—all using identical application code.
//!
//! Inspired by [FoundationDB's simulation testing](https://apple.github.io/foundationdb/testing.html).
//!
//! ## Crate Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │           moonpool (this crate)                 │
//! │         Re-exports all functionality            │
//! ├─────────────────────────────────────────────────┤
//! │  moonpool-transport    │    moonpool-sim        │
//! │  • Peer connections    │    • SimWorld runtime  │
//! │  • Wire format         │    • Chaos testing     │
//! │  • NetTransport        │    • Buggify macros    │
//! │  • RPC primitives      │    • Assertions        │
//! ├─────────────────────────────────────────────────┤
//! │              moonpool-core                      │
//! │  Provider traits: Time, Task, Network, Random   │
//! │  Core types: UID, Endpoint, NetworkAddress      │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! ## Quick Start
//!
//! ```ignore
//! use moonpool::{SimulationBuilder, WorkloadTopology};
//!
//! SimulationBuilder::new()
//!     .topology(WorkloadTopology::ClientServer { clients: 2, servers: 1 })
//!     .run(|ctx| async move {
//!         // Your distributed system workload
//!     });
//! ```
//!
//! ## Which Crate to Use
//!
//! | Use case | Crate |
//! |----------|-------|
//! | Full framework (recommended) | `moonpool` |
//! | Provider traits only | `moonpool-core` |
//! | Simulation without transport | `moonpool-sim` |
//! | Transport without simulation | `moonpool-transport` |
//!
//! ## Documentation
//!
//! - [`moonpool_core`] - Provider traits and core types
//! - [`moonpool_sim`] - Simulation runtime and chaos testing
//! - [`moonpool_transport`] - Network transport layer

#![deny(missing_docs)]

// Re-export all public items from sub-crates
pub use moonpool_core::*;
pub use moonpool_sim::*;
pub use moonpool_transport::*;
