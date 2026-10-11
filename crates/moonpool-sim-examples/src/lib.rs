//! Example simulation workloads for moonpool.
//!
//! Each module is a self-contained example: a system under test (a
//! [`Process`](moonpool_sim::Process)), a test driver (a
//! [`Workload`](moonpool_sim::Workload)), and a `sim-*` binary that wires
//! them into a `SimulationBuilder` with chaos. `cargo xtask sim` discovers
//! and runs the binaries. The crate is not published.
//!
//! The examples live apart from `moonpool-sim` so that sancov instruments
//! only the application code (about 2 to 3 thousand edges), not the
//! simulation framework (about 21 thousand edges). Coverage then measures
//! the example, not the simulator.
//!
//! # Quick example
//!
//! The `sim-journal` binary, reduced: one node owns a `moonpool-journal`
//! journal, attrition crashes it, and every boot checks that no acknowledged
//! write was lost.
//!
//! ```rust,no_run
//! use std::time::Duration;
//!
//! use moonpool_sim::{
//!     Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder,
//!     StorageFault, StorageFaultMask,
//! };
//! use moonpool_sim_examples::journal::{JournalNode, JournalWorkload};
//! use moonpool_sim_examples::support::finish_or_exit_on_failure;
//!
//! let report = SimulationBuilder::new()
//!     .processes(1, || Box::new(JournalNode))
//!     .workload(JournalWorkload)
//!     .enable_chaos([
//!         Chaos::Attrition {
//!             config: Attrition {
//!                 max_dead: 1,
//!                 prob_graceful: 0.2,
//!                 prob_crash: 0.8,
//!                 // A wiped disk is data loss by definition: never wipe.
//!                 prob_wipe: 0.0,
//!                 recovery_delay_ms: None,
//!                 grace_period_ms: None,
//!                 scope: AttritionScope::PerProcess,
//!                 victims: AttritionVictims::Any,
//!             },
//!             mode: ChaosMode::Random,
//!         },
//!         Chaos::Storage(ChaosMode::Random),
//!     ])
//!     // Damage that a lone node has no second copy to repair from.
//!     .storage_fault_mask(
//!         StorageFaultMask::all()
//!             .without(StorageFault::Corruption)
//!             .without(StorageFault::Eio)
//!             .without(StorageFault::Misdirect)
//!             .without(StorageFault::PhantomWrite)
//!             .without(StorageFault::DiskFailure),
//!     )
//!     .chaos_duration(Duration::from_secs(30))
//!     .set_debug_seeds(vec![42])
//!     .set_iterations(1)
//!     .run()
//!     .expect("simulation configuration is valid");
//! finish_or_exit_on_failure(&report);
//! ```
//!
//! # Examples
//!
//! | Module | Binary | What it shows |
//! |---|---|---|
//! | [`maze`] | `sim-maze-explore` | Four locks behind nested probability gates: found only with frontier exploration |
//! | [`dungeon`] | `sim-dungeon-explore` | A 10-floor roguelike: exploration climbs floor by floor from each discovery |
//! | [`journal`] | `sim-journal` | A `moonpool-journal` node under crash attrition and storage chaos, with a ledger of acknowledged writes |
//! | [`axum_web`] | `sim-axum-web` | An axum router served over simulated TCP through `moonpool-hyper` |
//! | [`tonic_grpc`] | `sim-tonic-grpc` | A tonic gRPC service without `tonic::transport`, on `moonpool-hyper` |
//! | [`metrics_service`] | `sim-metrics-service` | A key/value service instrumented with `prometheus`, reported through `moonpool-prometheus` |
//! | [`topology`] | `sim-topology` | A 3 datacenter, 3 zone, 3 machine cluster under machine-scoped attrition |
//! | [`support`] | | Shared plumbing: error helpers, an attrition preset, exit-code handling |
//!
//! Run one example, or every example:
//!
//! ```text
//! cargo xtask sim run journal
//! cargo xtask sim run-all
//! ```
//!
//! The `tonic_grpc` example compiles `proto/echo.proto` at build time with
//! `tonic-prost-build`, so `protoc` must be on the `PATH`.
//!
//! # Feature flags
//!
//! None.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod axum_web;
pub mod dungeon;
pub mod journal;
pub mod maze;
pub mod metrics_service;
pub mod operator;
pub mod support;
pub mod tonic_grpc;
pub mod topology;
