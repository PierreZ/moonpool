//! # moonpool-core
//!
//! Provider traits and core types for the moonpool simulation framework.
//!
//! A distributed system touches the outside world through a few operations:
//! time, task spawning, networking, randomness, storage and system counters.
//! This crate puts each one behind a trait. Application code depends on the
//! traits, never on tokio. In production the code runs on [`TokioProviders`].
//! In tests `moonpool-sim` supplies deterministic providers, and the code
//! does not change. This is `FoundationDB`'s rule: no mocks and no `Sim*`
//! variants of shipped types, the same code in both places.
//!
//! ```text
//!                 application code
//!        (generic over P: Providers, never tokio)
//!                         │
//!          ┌──────────────┴──────────────┐
//!          ▼                             ▼
//!   TokioProviders                 SimProviders
//!   (this crate,                   (moonpool-sim,
//!    real I/O)                      deterministic)
//! ```
//!
//! ## Provider traits
//!
//! | Trait | Production (`Tokio*`) | Simulation (`moonpool-sim`) | Purpose |
//! |-------|-----------------------|-----------------------------|---------|
//! | [`TimeProvider`] | Wall clock | Logical time, clock drift | `sleep`, `timeout`, `now`, `timer` |
//! | [`TaskProvider`] | `tokio::spawn` | Seeded deterministic executor | `spawn_task`, `yield_now`; `spawn_local` via [`LocalTaskProvider`] |
//! | [`NetworkProvider`] | Real TCP (`TCP_NODELAY`) | Simulated TCP with faults | `bind`, `connect`, `accept` |
//! | [`RandomProvider`] | Thread-local RNG | The run's one seeded stream | `random`, `random_range`, `random_bool` |
//! | [`StorageProvider`] | Real filesystem, direct I/O | Simulated disk with faults | `open`, `rename`, `sync_dir`, positioned I/O |
//! | [`SystemProvider`] | `getrusage`, `/proc` | Simulated CPU and disk counters | Cumulative CPU and disk busyness |
//! | [`Resolver`] (outside the bundle) | OS lookup | Scripted table | `host:port` to addresses |
//!
//! [`Providers`] bundles the six provider types into one type parameter.
//!
//! ## Writing code against the providers
//!
//! The function below never names a runtime. Production calls it with
//! [`TokioProviders`]. A simulation test calls it with the simulator's
//! bundle, where the sleep advances logical time and the draws come from
//! the seed.
//!
//! ```rust
//! use std::time::Duration;
//!
//! use moonpool_core::TimeError;
//! use moonpool_core::prelude::*;
//!
//! async fn backoff<P: Providers>(providers: &P, attempt: u32) -> Result<(), TimeError> {
//!     let base_ms = 10_u64 << attempt;
//!     let jitter_ms = providers.random().random_range(0..base_ms);
//!     providers.time().sleep(Duration::from_millis(base_ms + jitter_ms)).await
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), TimeError> {
//! let providers = TokioProviders::new();
//! backoff(&providers, 0).await?;
//! # Ok(())
//! # }
//! ```
//!
//! **Never call tokio directly in application code.** Use
//! `providers.time().sleep()`, not `tokio::time::sleep()`, and use
//! [`select!`] (re-exported by `moonpool-sim` and `moonpool`), not
//! `tokio::select!`.
//!
//! ## Local (non-Send) code
//!
//! Thread-per-core code that keeps `Rc<RefCell<…>>` across awaits bounds on
//! [`LocalProviders`] instead of [`Providers`]. Its task provider implements
//! [`LocalTaskProvider`], so `providers.task().spawn_local(..)` accepts
//! futures that are not `Send`, and [`TimeProvider::timeout_local`] bounds
//! them. In production this is `CompioProviders` from `moonpool-compio`; in
//! simulation it is `SimProviders`. [`TokioProviders`] does not satisfy the
//! bound, so this code cannot land on the work-stealing runtime by mistake.
//!
//! The example below only compiles generic code. See `moonpool-compio` for a
//! runnable version.
//!
//! ```rust
//! use std::{cell::RefCell, rc::Rc, time::Duration};
//!
//! use moonpool_core::prelude::*;
//! use moonpool_core::{JoinError, TimeError};
//!
//! async fn count_once<P: LocalProviders>(providers: &P, hits: Rc<RefCell<u64>>) -> Result<(), JoinError> {
//!     providers
//!         .task()
//!         .spawn_local("count", async move { *hits.borrow_mut() += 1 })
//!         .await
//! }
//!
//! async fn read_within<P: LocalProviders>(providers: &P, hits: Rc<RefCell<u64>>) -> Result<u64, TimeError> {
//!     providers
//!         .time()
//!         .timeout_local(Duration::from_secs(1), async move { *hits.borrow() })
//!         .await
//! }
//! # fn main() {}
//! ```
//!
//! ## Other core types
//!
//! - Storage: [`OpenOptions`] with [`DirectIo`], [`StorageFile`] positioned
//!   I/O and sync barriers, [`IoConstraints`] and [`AlignedBuf`] for direct
//!   I/O (an owned [`AlignedBuf`] is also the unit of zero-copy positioned
//!   I/O: [`StorageFile::write_at_aligned`] and its siblings), [`BlockFile`]
//!   for block addressing, and [`LayoutRegion`] so a
//!   storage format can tell a fault injector where its bytes live.
//! - System: [`SystemSample`] and [`DiskCounters`], with the
//!   [`parse_proc_stat`] and [`parse_diskstats`] parsers.
//! - Metrics: [`MetricsSource`], the registry-agnostic seam the simulation
//!   report scrapes, and the [`metrics::query`] engine.
//! - Errors: [`SimulationError`], [`TimeError`] and [`JoinError`].
//!
//! ## Cargo features
//!
//! | Feature | Default | Adds |
//! |---------|---------|------|
//! | `tokio-providers` | yes | [`TokioProviders`]: all six `tokio-*` features below |
//! | `tokio-task` | via umbrella | [`TokioTaskProvider`] |
//! | `tokio-time` | via umbrella | [`TokioTimeProvider`] |
//! | `tokio-net` | via umbrella | [`TokioNetworkProvider`] and [`TokioResolver`] |
//! | `tokio-fs` | via umbrella | [`TokioStorageProvider`], with `O_DIRECT` support |
//! | `thread-rng` | via umbrella | [`ThreadRngRandomProvider`] |
//! | `tokio-system` | via umbrella | [`TokioSystemProvider`] |
//! | `host` | via `tokio-fs` / `tokio-system` | [`direct_io`] and [`host`]: the direct-I/O probing and `/proc`, `getrusage`, `statvfs` readers shared by the production backends |
//! | `select` | yes | [`select!`] as tokio's macro, unchanged |
//! | `deterministic-select` | no | [`select!`] with a seeded branch offset (`moonpool-sim` turns it on) |
//!
//! With `default-features = false` the crate holds only the traits and plain
//! types, and it compiles for `wasm32-unknown-unknown`.
//!
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

mod block;
mod error;
pub mod metrics;
mod network;
mod providers;
mod random;
mod resolver;
#[cfg(feature = "deterministic-select")]
mod select;
#[cfg(feature = "deterministic-select")]
pub mod select_support;
mod storage;
mod system;
mod task;
mod time;

/// `select!` as tokio's macro, verbatim (production passthrough).
///
/// With the `deterministic-select` feature enabled (moonpool-sim does this),
/// this re-export is replaced by the seeded-offset macro defined in
/// [`select`](crate::select!); both are tokio's expansion, so the grammar
/// and limits are identical either way.
#[cfg(all(feature = "select", not(feature = "deterministic-select")))]
pub use tokio::select;

/// tokio re-export used by `select!` expansions so downstream crates never
/// need a direct tokio dependency for the macro to resolve. Not public API.
#[cfg(feature = "select")]
#[doc(hidden)]
pub use tokio as __tokio;

// The block-addressed view of one open file.
pub use block::BlockFile;

// Error exports
pub use error::{SimulationError, SimulationResult};

// Application metrics seam (registry-agnostic; adapters live in their own crates).
pub use metrics::{HistogramValue, MetricSample, MetricValue, MetricsSource};

// Provider trait exports
pub use network::{NetworkProvider, TcpListenerTrait};
#[cfg(feature = "tokio-net")]
pub use network::{TokioNetworkProvider, TokioTcpListener};
#[cfg(feature = "tokio-providers")]
pub use providers::TokioProviders;
pub use providers::{LocalProviders, Providers};
pub use random::RandomProvider;
#[cfg(feature = "thread-rng")]
pub use random::ThreadRngRandomProvider;
#[cfg(feature = "tokio-net")]
pub use resolver::TokioResolver;
pub use resolver::{Resolver, split_host_port};
#[cfg(feature = "host")]
pub use storage::direct_io;
pub use storage::{
    AlignedBuf, DirectIo, InvalidOpenOptions, IoConstraints, LayoutRegion, OpenOptions,
    StorageFile, StorageProvider, stream_io_unsupported,
};
#[cfg(feature = "tokio-fs")]
pub use storage::{TokioStorageFile, TokioStorageProvider};
#[cfg(feature = "tokio-system")]
pub use system::TokioSystemProvider;
#[cfg(feature = "host")]
pub use system::host;
pub use system::{
    DiskCounters, DiskSpace, SystemProvider, SystemSample, parse_diskstats, parse_proc_stat,
};
#[doc(hidden)]
pub use task::traced_task;
pub use task::{Detach, JoinError, LocalTaskProvider, TaskProvider};
#[cfg(feature = "tokio-task")]
pub use task::{TokioJoinHandle, TokioTaskProvider};
#[cfg(feature = "tokio-time")]
pub use time::TokioTimeProvider;
pub use time::{TimeError, TimeProvider};

/// Common imports for writing code against the provider traits.
///
/// Bringing the provider traits into scope is the usual ergonomic need — their
/// `async fn`s (RPIT) are only callable with the trait in scope. Glob-import this
/// in production code:
///
/// ```
/// use moonpool_core::prelude::*;
/// ```
pub mod prelude {
    pub use crate::{
        LocalProviders, LocalTaskProvider, NetworkProvider, Providers, RandomProvider,
        StorageProvider, SystemProvider, TaskProvider, TcpListenerTrait, TimeProvider,
    };

    #[cfg(feature = "tokio-providers")]
    pub use crate::TokioProviders;
}
