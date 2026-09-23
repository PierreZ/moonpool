//! The `sim-rpc-streams` campaign (#216): reply streams with
//! consumption-based credit, abandonment, saturation and bounded admission
//! under faults.
//!
//! Roles:
//!
//! - **producer** ([`StreamsProducer`]): a streaming `ScanItems` endpoint
//!   whose handlers stream items as each request asks (a count, an item
//!   size, pauses, an ending: finish, fail with a code, drop, or send until
//!   closed) and write the producer ledger as they go; a unary `Ping`; a
//!   well-known directory. A graceful shutdown fails its open streams with
//!   [`SHUTDOWN_CODE`](messages::SHUTDOWN_CODE) and keeps its runtime up
//!   for a short grace.
//! - **workload** ([`StreamsWorkload`]): a client-only runtime that opens
//!   streams and consumes them eagerly, slowly or not at all; abandons them
//!   before and after their first item; times out on them; saturates their
//!   credit while checking that a unary call, a new stream and a cancel
//!   still get through; bursts calls and streams against squeezed budgets;
//!   asks for producer crashes and graceful shutdowns mid-stream; and at
//!   the end, after the faults stopped, requires a fresh stream and a unary
//!   call to succeed.
//!
//! **Oracle.** Two external ledgers ([`state::StreamLedger`]): the producer
//! records each stream's boot, every item it queued with its accounted size
//! and how it ended; the workload records every item its application took.
//! Every consumed item must be the produced item at the same position
//! (order, no gaps, no repeats); a normal end must follow every item its
//! producer sent, a producer's failure likewise; a refusal before
//! admission never reached a producer, and a refused probe never ran; an
//! abandoned stream's producer must stop (no orphan); a producer never runs
//! further ahead of consumption than its window (plus the one item handed
//! to a waiting reader). Nothing reads the transport's queues or credit to
//! decide what should have happened.
//!
//! **Faults.** Swarm network chaos with knob spikes, the delivery
//! campaign's buggified peer policy, squeezed admission and stream budgets
//! ([`streams_config`]), and a script ([`StreamsFaults`]) that crashes the
//! producer or shuts it down gracefully when the workload asks, mid-stream.

pub mod faults;
pub mod messages;
mod policy;
mod producer;
pub mod state;
mod workload;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_sim::{Chaos, ChaosMode, SimulationBuilder};

pub use faults::StreamsFaults;
pub use policy::streams_config;
pub use producer::StreamsProducer;
pub use workload::{StreamOp, StreamsConfig, StreamsWorkload};

pub(crate) use crate::foundations::{RPC_PORT, report_stats};

/// Sleep on simulation time; an error means the simulation is going away.
pub(crate) async fn pause(
    ctx: &moonpool_sim::SimContext,
    duration: Duration,
) -> moonpool_sim::SimulationResult<()> {
    moonpool_sim::TimeProvider::sleep(ctx.time(), duration)
        .await
        .map_err(|error| {
            moonpool_sim::SimulationError::InvalidState(format!("sleep failed: {error}"))
        })
}

/// One finished run as the workload saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamsRecord {
    /// One line per stream and discovery: id, operation, items taken,
    /// outcome.
    pub history: Vec<String>,
}

/// Where finished runs are appended, for tests to inspect.
pub type StreamsRecords = Arc<Mutex<Vec<StreamsRecord>>>;

/// The campaign's builder without an iteration policy.
#[must_use]
pub fn streams_campaign(config: StreamsConfig, records: &StreamsRecords) -> SimulationBuilder {
    let records = Arc::clone(records);
    SimulationBuilder::new()
        .processes(1, || Box::new(StreamsProducer))
        .workload_factory(move || {
            Box::new(StreamsWorkload::new(config.clone(), Arc::clone(&records)))
        })
        .fault_factory(|| Box::new(StreamsFaults::default()))
        .enable_chaos([Chaos::Network(ChaosMode::Swarm), Chaos::BuggifyKnobs])
        .chaos_duration(Duration::from_secs(8))
}
