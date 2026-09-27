//! # moonpool-rpc-sim
//!
//! The deterministic-simulation side of `moonpool-rpc`: process and workload
//! definitions, independent oracles and the campaigns CI runs. It is not
//! published, and the production crate never depends on it.
//!
//! - [`foundations`]: the `sim-rpc-foundations` campaign (#213): typed
//!   dynamic-endpoint request/reply between two process groups and a
//!   surviving workload, judged by a handler receipt ledger kept apart from
//!   the transport.
//! - [`delivery`]: the `sim-rpc-delivery` campaign (#214): every delivery
//!   mode, lost replies, crashes, simultaneous connects, bootstrap names
//!   and failure-monitor transitions, judged by an execution ledger.
//! - [`interfaces`]: the `sim-rpc-interfaces` campaign (#215): interfaces
//!   published, stored, recruited and forwarded to a third participant
//!   across repeated same-address restarts, judged by boot, publication
//!   and execution ledgers.
//! - [`balance`]: the `sim-rpc-balance` campaign (#217): balanced calls
//!   over explicit alternative sets under separate retry and duplicate
//!   permissions, hedging, comparison copies, cancellation, late losers
//!   and outages of every alternative, judged by receipt and attempt
//!   ledgers.
//! - [`streams`]: the `sim-rpc-streams` campaign (#216): reply streams with
//!   consumption-based credit, abandonment, saturation, bounded admission
//!   and producer reboots, judged by producer and consumer ledgers.
//! - [`security`]: the `sim-rpc-security` campaign (#218): credential
//!   verification and endpoint access under key rotation, UTC transitions,
//!   crashes, graceful shutdowns and mixed protocol versions, judged by an
//!   issue/receipt ledger: no unauthorized execution, ever.
//!
//! - [`qualification`]: the `sim-rpc-qualification` campaign (#219): all of
//!   the above at once — Paros-style publication and same-address reboots,
//!   reliable ambiguity, streams, balancing, credentials, versions,
//!   graceful shutdown and overload — with a declared recovery phase,
//!   resource baselines after every process death and at the end, and
//!   semantic replay across runs and host speeds.
//!
//! Only the foundations campaign draws the simulator's bit flips (next to
//! its own corrupting wire): it is the corruption suite. Every other
//! campaign runs [`without_corruption`].
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
