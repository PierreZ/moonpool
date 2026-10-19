//! compio-backed providers for moonpool: the production runtime for
//! thread-per-core deployments.
//!
//! [compio](https://docs.rs/compio) is a completion-based, thread-per-core
//! runtime: `io_uring` on Linux (epoll when `io_uring` is unavailable), one
//! runtime per thread, no work stealing. This crate implements moonpool's
//! six provider traits and [`Resolver`](moonpool_core::Resolver) on it, so
//! code written against [`Providers`](moonpool_core::Providers), and tested
//! under `moonpool-sim`, runs on compio unchanged:
//!
//! | Provider | Type |
//! |----------|------|
//! | network | [`CompioNetworkProvider`] |
//! | time | [`CompioTimeProvider`] |
//! | task | [`CompioTaskProvider`] (also a [`LocalTaskProvider`](moonpool_core::LocalTaskProvider)) |
//! | random | `moonpool_core::TokioRandomProvider` (rand's thread-local generator, no Tokio) |
//! | storage | [`CompioStorageProvider`] |
//! | system | [`CompioSystemProvider`] |
//! | resolver | [`CompioResolver`] |
//!
//! [`CompioProviders`] bundles the six.
//!
//! # Starting
//!
//! Run one compio runtime per thread (per core) and build the providers
//! inside it:
//!
//! ```no_run
//! use moonpool_compio::CompioProviders;
//! use moonpool_core::{Providers, TimeProvider};
//!
//! fn main() -> std::io::Result<()> {
//!     compio::runtime::Runtime::new()?.block_on(async {
//!         let providers = CompioProviders::new();
//!         providers
//!             .time()
//!             .sleep(std::time::Duration::from_millis(1))
//!             .await
//!             .map_err(std::io::Error::other)?;
//!         Ok(())
//!     })
//! }
//! ```
//!
//! `#[compio::main]` (compio's `macros` feature) does the same for `main`.
//!
//! # The thread-affinity contract
//!
//! moonpool's provider traits return `Send` futures and a `Send` TCP stream,
//! so the same application code runs on Tokio's work-stealing scheduler. Every
//! compio future is `!Send`: it holds the thread's driver through an `Rc`.
//! This crate satisfies `Send` with a thread-affinity wrapper
//! (`compio_send_wrapper::SendWrapper`) around each of them, and the wrapper
//! enforces the rule that makes that sound:
//!
//! **every future a provider returns, and every [`CompioTcpStream`], must be
//! polled and dropped on the runtime thread that created it.** Polling one on
//! another thread panics. Dropping one on another thread panics too, and
//! leaks the inner value instead of dropping it there, because dropping
//! compio state off its thread is exactly the unsoundness the wrapper exists
//! to prevent.
//!
//! On compio nothing ever moves a task between threads, so code that awaits
//! provider futures inside its own tasks meets the rule without trying. What
//! breaks it is handing a future or a stream to another thread (a channel to
//! another core, `std::thread::spawn`). The handle types themselves (the
//! listener, the storage file at rest, the join handle) are genuinely
//! `Send + Sync`.
//!
//! # Costs and gaps
//!
//! - **TCP streams copy once per direction** through compio's compat layer
//!   (an 8 KiB buffer that grows on demand), and **written bytes leave on
//!   flush**: see [`CompioTcpStream`].
//! - **Durable writes are a write plus `sync_data`**: compio has no per-write
//!   `RWF_DSYNC`, which the Tokio backend uses. See [`CompioStorageFile`].
//! - **Stream I/O on files is emulated** over positioned operations, and
//!   append mode is not atomic against concurrent appenders. See
//!   [`CompioStorageFile`].
//!
//! # `!Send` tasks
//!
//! Thread-per-core code that keeps its state in `Rc<RefCell<…>>` spawns it
//! with [`LocalTaskProvider::spawn_local`](moonpool_core::LocalTaskProvider::spawn_local)
//! and bounds on [`LocalProviders`](moonpool_core::LocalProviders), which
//! [`CompioProviders`] satisfies.

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

mod buf;
mod net;
mod providers;
mod resolver;
mod storage;
mod system;
mod task;
mod time;

pub use net::{CompioNetworkProvider, CompioTcpListener, CompioTcpStream};
pub use providers::CompioProviders;
pub use resolver::CompioResolver;
pub use storage::{CompioStorageFile, CompioStorageProvider};
pub use system::CompioSystemProvider;
pub use task::{CompioJoinHandle, CompioTaskProvider};
pub use time::CompioTimeProvider;
