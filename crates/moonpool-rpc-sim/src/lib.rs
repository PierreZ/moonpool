//! # moonpool-rpc-sim
//!
//! The deterministic-simulation side of `moonpool-rpc`: processes,
//! workloads, independent oracles and the campaigns that CI runs. This
//! crate is not published. The production crate never depends on it, nor
//! on `moonpool-sim`, even through a feature.
//!
//! Each campaign runs the **same** `moonpool-rpc` code that runs on real
//! TCP. The examples (`tcp_echo`, `tcp_latency`, `balance_latency`,
//! `rpc_soak`) run that code on `TokioProviders`. The oracles never read the
//! transport: handlers record what they executed in a ledger kept in the
//! run's state, and the workload judges its own outcomes against it.
//!
//! # Quick example
//!
//! Replay one seed of the foundations campaign, as a debugging session
//! would:
//!
//! ```rust,no_run
//! use std::sync::{Arc, Mutex};
//!
//! use moonpool_rpc_sim::foundations::{WorkloadConfig, campaign};
//!
//! let observations = Arc::new(Mutex::new(Vec::new()));
//! let report = campaign(WorkloadConfig::campaign(), &observations)
//!     .set_debug_seeds(vec![42])
//!     .set_iterations(1)
//!     .run()
//!     .expect("simulation configuration is valid");
//! assert!(report.seeds_failing.is_empty());
//! ```
//!
//! The binaries run each campaign until coverage is stable, under the
//! determinism canary:
//!
//! ```text
//! cargo xtask sim run rpc-foundations
//! cargo xtask sim run rpc
//! ```
//!
//! # Campaigns
//!
//! | Module | Binary | What it proves |
//! |---|---|---|
//! | [`foundations`] | `sim-rpc-foundations` | Typed request/reply between two process groups and a surviving workload, judged by a handler receipt ledger. The corruption suite: it alone draws bit flips |
//! | [`delivery`] | `sim-rpc-delivery` | Every delivery mode, lost replies, crashes, simultaneous connects, bootstrap names and failure-monitor transitions, judged by an execution ledger |
//! | [`interfaces`] | `sim-rpc-interfaces` | Interfaces published, stored, recruited and forwarded to a third participant across same-address restarts |
//! | [`balance`] | `sim-rpc-balance` | Balanced calls under separate retry and duplicate permissions, hedging, cancellation, late losers and outages of every alternative |
//! | [`streams`] | `sim-rpc-streams` | Reply streams with consumption credit, abandonment, saturation, bounded admission and producer reboots |
//! | [`security`] | `sim-rpc-security` | Credentials and endpoint access under key rotation, UTC transitions, crashes, graceful shutdowns and mixed protocol versions: no unauthorized execution, ever |
//! | [`qualification`] | `sim-rpc-qualification` | All of the above at once, with a declared recovery phase, resource baselines after every process death, and semantic replay across runs and host speeds |
//!
//! Each module exposes a campaign function that returns a configured
//! `moonpool_sim::SimulationBuilder`, plus its processes, workload, fault
//! injector and records. The tests in `tests/` run each campaign as
//! bounded, deterministic seed budgets under nextest.
//!
//! # Faults
//!
//! - **Network chaos.** Swarm network faults per seed. Every campaign but
//!   foundations masks bit flips with [`without_corruption`]: corruption is
//!   its own explicit suite, never arbitrary loss on healthy TCP.
//! - **Process faults.** Scripted crashes (for example, right after a
//!   handler received a request), same-address restarts and graceful
//!   shutdowns, through each campaign's `FaultInjector`.
//! - **Runtime buggify sites.** `moonpool-rpc` carries its own buggify
//!   points: session cuts behind a request, spurious overload refusals,
//!   deferred stream acknowledgements and spurious failure-monitor
//!   wakeups. Each campaign records which ones fired as reachable
//!   assertions, read from the counters of `moonpool_rpc::RpcStats`.
//!
//! # Feature flags
//!
//! None. The crate enables the `derive` and `jwt` features of
//! `moonpool-rpc`: the interfaces campaign serves a generated interface,
//! and the security campaign verifies real signed tokens.
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod balance;
pub mod delivery;
pub mod foundations;
pub mod interfaces;
pub mod qualification;
pub mod security;
pub mod streams;

use moonpool_sim::{NetworkFault, NetworkFaultMask};

/// Record which of `moonpool-rpc`'s own buggify sites fired in a runtime, one
/// reachable per site, from the counters it exposes for them. A site is a
/// cause, not an outcome, so it is a reachable and never a sometimes: whether
/// a seed activates it is the draw's business. The outcomes it forces are the
/// campaigns' own oracles and scenarios (an ambiguous at-most-once failure, a
/// reliable call executed twice, an overload refusal, a producer waiting for
/// credit, a failure-monitor wait that re-checks).
fn observe_injections(stats: &moonpool_rpc::RpcStats) {
    if stats.injected_request_cuts > 0 {
        moonpool_sim::assert_reachable!("rpc buggify: session cut behind an at-most-once request");
    }
    if stats.injected_reliable_request_cuts > 0 {
        moonpool_sim::assert_reachable!("rpc buggify: session cut behind a reliable request");
    }
    if stats.injected_overloads > 0 {
        moonpool_sim::assert_reachable!("rpc buggify: request refused overloaded at admission");
    }
    if stats.injected_ack_deferrals > 0 {
        moonpool_sim::assert_reachable!("rpc buggify: stream acknowledgement deferred");
    }
    if stats.injected_spurious_wakeups > 0 {
        moonpool_sim::assert_reachable!("rpc buggify: failure-monitor wakeup with nothing changed");
    }
}

/// Lock a campaign's shared state. Poisoning means a prior task panicked.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("Mutex poisoned: prior task panicked")
}

/// Every network fault family except in-flight bit flips.
///
/// Corruption is a separate, explicit network-model campaign (the
/// foundations campaign's corrupting wire and its
/// `network_bit_flips_are_caught_by_frame_checksums` suite), never
/// arbitrary message loss on healthy TCP: the campaigns that judge
/// delivery, interfaces, streams, balancing, security and qualification
/// run on every other family (partitions, clogs, random closes, connect
/// failures, black holes, latency, clock drift) and mask this one.
#[must_use]
pub fn without_corruption() -> NetworkFaultMask {
    NetworkFaultMask::all().without(NetworkFault::BitFlip)
}
