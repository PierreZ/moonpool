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
//! ┌─────────────────────────────────────────────────────────────┐
//! │              moonpool (this crate)                          │
//! │   Re-exports all functionality from sub-crates             │
//! ├─────────────────────────────────────────────────────────────┤
//! │                      moonpool-sim                           │
//! │  • SimWorld runtime         • Chaos testing                 │
//! │  • Buggify macros           • Multiverse exploration       │
//! │                               (via moonpool-explorer)       │
//! ├─────────────────────────────────────────────────────────────┤
//! │       moonpool-hyper (feature "hyper", opt-in)              │
//! │  • hyper runtime adapters   • HyperIo over provider streams │
//! │  • Reconnecting h2 channel  • Per-connection serve helper   │
//! ├─────────────────────────────────────────────────────────────┤
//! │                     moonpool-core                           │
//! │  Provider traits: Time, Task, Network, Random, Storage      │
//! └─────────────────────────────────────────────────────────────┘
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
//! | Simulation runtime | `moonpool-sim` |
//! | An HTTP/2 stack (tonic, axum, hyper) on the providers | `moonpool` with feature `hyper`, or `moonpool-hyper` |
//! | Fork-based exploration internals | `moonpool-explorer` |
//!
//! ## Documentation
//!
//! - [`moonpool_core`] - Provider traits and core types
//! - [`moonpool_sim`] - Simulation runtime and chaos testing
//! - `moonpool::hyper` - hyper 1.x integration, behind the `hyper` feature

#![deny(missing_docs)]
#![allow(ambiguous_glob_reexports)]

// Re-export all public items from sub-crates. `moonpool-core` is always present;
// `sim` is feature-gated so a lean production build pulls neither the simulation
// runtime nor the explorer (no libc/mio in the prod dependency tree).
pub use moonpool_core::*;
#[cfg(feature = "sim")]
pub use moonpool_sim::*;

/// hyper 1.x integration, from [`moonpool_hyper`].
///
/// A namespaced module rather than a fourth glob re-export at the root: the
/// three globs above already collide in places (hence
/// `allow(ambiguous_glob_reexports)`), and hyper names like `H2Channel` or
/// `KeepAlive` read better qualified anyway.
///
/// ```ignore
/// use moonpool::hyper::{ChannelConfig, ReconnectingChannel};
/// ```
///
/// Contains the runtime adapters hyper needs (executor and timer over the task
/// and time providers), `HyperIo` to present a provider stream as hyper IO, a
/// reconnecting h2 client channel in the shape tower and tonic expect, and a
/// per-connection serve helper. Requires the `hyper` feature.
#[cfg(feature = "hyper")]
pub mod hyper {
    pub use moonpool_hyper::*;
}

/// Common imports for application code.
///
/// Production:
///
/// ```
/// use moonpool::prelude::*;
/// ```
///
/// Brings the provider traits into scope (needed to call their async methods)
/// and, when the `sim` feature is on, the simulation builder/driver types.
pub mod prelude {
    pub use moonpool_core::prelude::*;

    #[cfg(feature = "sim")]
    pub use moonpool_sim::{Process, SimContext, SimulationBuilder, Workload, WorkloadTopology};
}
