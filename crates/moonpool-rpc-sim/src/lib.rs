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
//! Only the foundations campaign draws the simulator's bit flips (next to
//! its own corrupting wire): it is the corruption suite. Every other
//! campaign runs [`without_corruption`].
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod balance;
pub mod delivery;
pub mod foundations;
pub mod interfaces;
pub mod security;
pub mod streams;

use moonpool_sim::{NetworkFault, NetworkFaultMask};

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
