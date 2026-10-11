//! The simulated providers: the simulation side of moonpool's provider
//! pattern.
//!
//! Shipped code depends on the provider traits of `moonpool-core`, never on
//! a concrete runtime. Production passes `TokioProviders`; the simulation
//! passes [`SimProviders`], one bundle per process, scoped to its IP.
//!
//! | Trait | Simulated provider | Backed by |
//! |-------|--------------------|-----------|
//! | [`TimeProvider`](crate::TimeProvider) | [`SimTimeProvider`] | Logical time of the [`SimWorld`](crate::SimWorld); `timer()` may drift ahead under network chaos |
//! | [`TaskProvider`](crate::TaskProvider) | [`SimTaskProvider`] | The deterministic [`executor`](crate::executor); spawned tasks inherit the process as owner |
//! | [`NetworkProvider`](crate::NetworkProvider) | [`SimNetworkProvider`](crate::SimNetworkProvider) | The network engine ([`network`](crate::network)) |
//! | [`RandomProvider`](crate::RandomProvider) | [`SimRandomProvider`] | The one seeded random stream ([`sim::rng`](crate::sim::rng)) |
//! | [`StorageProvider`](crate::StorageProvider) | [`SimStorageProvider`](crate::SimStorageProvider) | The storage engine ([`storage`](crate::storage)) |
//! | [`SystemProvider`](crate::SystemProvider) | [`SimSystemProvider`] | Deterministic disk counters per process |
//!
//! [`ScriptedResolver`] is a [`Resolver`](crate::Resolver) that answers
//! name lookups from a table the simulation edits while it runs.

mod random;
mod resolver;
mod sim_providers;
mod system;
mod task;
mod time;

pub use random::SimRandomProvider;
pub use resolver::ScriptedResolver;
pub use sim_providers::SimProviders;
pub use system::SimSystemProvider;
pub use task::SimTaskProvider;
pub(crate) use task::{TaskPanicReporter, TaskPanicTracker, panic_message};
pub use time::SimTimeProvider;
