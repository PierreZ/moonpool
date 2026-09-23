//! The `sim-rpc-delivery` campaign (#214): delivery modes, peer recovery
//! and failure monitoring under faults.
//!
//! Roles:
//!
//! - **server** ([`DeliveryServer`]): a dynamic `Execute` endpoint whose
//!   handler records every execution in the ledger *first*, then replies,
//!   replies late, declines to reply, breaks its promise, answers into a
//!   scripted cut, or asks to be crashed; plus a well-known `Directory`
//!   answering in every incarnation.
//! - **peer** ([`DeliveryPeer`], two of them): listening runtimes that call
//!   each other at the same instants, so both dial at once.
//! - **workload** ([`DeliveryWorkload`]): a client-only runtime that
//!   survives crashes, bootstraps through a scripted name, drives every
//!   delivery mode and watches the failure monitor.
//!
//! **Oracle.** The workload generates every job id; the handler's ledger
//! counts executions independently of the transport. A single attempt
//! (`try_get_reply`, one-way, attempts) never shows two receipts; a reply
//! shows at least one; a `NotAdmitted` error shows none; reliable delivery
//! may show several and the campaign requires that it sometimes does.
//!
//! **Faults.** Swarm network chaos with knob spikes, every
//! [`PeerPolicy`](moonpool_rpc::PeerPolicy) field a `buggify_knob!`, and a
//! scripted injector ([`DeliveryFaults`]): the bootstrap name is missing,
//! then points nowhere, then at the server; a lost-reply job partitions
//! the workload from the server right after execution until the client
//! notices; a crash job crashes and restarts the server at the same
//! address.

mod faults;
pub mod messages;
mod peer;
mod policy;
mod server;
pub mod state;
mod workload;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_sim::{Chaos, ChaosMode, SimulationBuilder};

pub use faults::DeliveryFaults;
pub use peer::DeliveryPeer;
pub use peer::DialGate;
pub use policy::{delivery_config, delivery_config_sharing, sharing_from};
pub use server::DeliveryServer;
pub use workload::{DeliveryConfig, DeliveryOp, DeliveryWorkload};

pub(crate) use crate::foundations::RPC_PORT;
use crate::foundations::report_stats;

/// One finished run as the workload saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRecord {
    /// One line per operation: id, operation, outcome.
    pub history: Vec<String>,
    /// Reliable jobs that executed more than once.
    pub duplicates: u32,
}

/// Where finished runs are appended, for tests to inspect.
pub type DeliveryRecords = Arc<Mutex<Vec<DeliveryRecord>>>;

/// The campaign's builder without an iteration policy.
#[must_use]
pub fn delivery_campaign(config: DeliveryConfig, records: &DeliveryRecords) -> SimulationBuilder {
    let records = Arc::clone(records);
    SimulationBuilder::new()
        .processes(1, || Box::new(DeliveryServer))
        .processes(2, || Box::new(DeliveryPeer))
        .workload_factory(move || {
            Box::new(DeliveryWorkload::new(config.clone(), Arc::clone(&records)))
        })
        .fault_factory(|| Box::new(DeliveryFaults::default()))
        .enable_chaos([Chaos::Network(ChaosMode::Swarm), Chaos::BuggifyKnobs])
        .chaos_duration(Duration::from_secs(8))
}
