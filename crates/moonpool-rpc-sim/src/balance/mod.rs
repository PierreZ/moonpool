//! The `sim-rpc-balance` campaign (#217): balanced calls over explicit
//! alternative sets, with separate retry and duplicate permissions,
//! hedging, comparison copies, cancellation and late losers.
//!
//! Roles:
//!
//! - **balance-server** ([`BalanceServer`], three of them): one runtime
//!   per boot, a `Work` endpoint published in the ledger's directory, a
//!   per-boot character (fast or slow with a penalty, faithful or
//!   divergent), and per-job behaviour (answer, decline as temporarily
//!   behind, break the promise, or hold the reply past every deadline).
//!   It destroys and republishes its endpoint now and then.
//! - **workload** ([`BalanceWorkload`]): one client runtime and one
//!   [`BalancedClient`](moonpool_rpc::balance::BalancedClient) that
//!   survives every fault. It learns references only from the directory
//!   and installs them as strictly newer sets; it never sees a refreshed
//!   reference otherwise.
//!
//! **Oracles**, written outside the balancer: the receipt ledger (every
//! handler run, by job id, written before the handler answers) and the
//! workload's attempt ledger (fed by the balancer's observation hook). The
//! workload checks that each job ran no more often than its permissions
//! allow, that a reply comes from a server that ran the job, that "not
//! admitted" means no handler ran, that late losers stay within the
//! model's bound, and at the end that every attempt started, ended,
//! reserved and released exactly once.
//!
//! **Faults** ([`BalanceFaults`]): every alternative taken away and
//! restored (a partition that keeps incarnations, or a crash of every
//! server that does not), single-server crashes, destroyed endpoints next
//! to healthy ones, swarm network chaos with knob spikes.

mod faults;
pub mod messages;
mod server;
pub mod state;
mod workload;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_sim::{Chaos, ChaosMode, SimulationBuilder};

pub use faults::BalanceFaults;
pub use server::BalanceServer;
pub use workload::{BalanceCampaignConfig, BalanceOp, BalanceWorkload};

/// One finished run as the workload saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceRecord {
    /// One line per operation: job id, operation, outcome.
    pub history: Vec<String>,
    /// Balanced calls made.
    pub calls: u64,
    /// A call succeeded once the faults were over.
    pub recovered: bool,
}

/// Where finished runs are appended, for tests to inspect.
pub type BalanceRecords = Arc<Mutex<Vec<BalanceRecord>>>;

/// The campaign's builder without an iteration policy.
#[must_use]
pub fn balance_campaign(
    config: BalanceCampaignConfig,
    records: &BalanceRecords,
) -> SimulationBuilder {
    let records = Arc::clone(records);
    SimulationBuilder::new()
        .processes(3, || Box::new(BalanceServer))
        .workload_factory(move || {
            Box::new(BalanceWorkload::new(config.clone(), Arc::clone(&records)))
        })
        .fault_factory(|| Box::new(BalanceFaults))
        .enable_chaos([Chaos::Network(ChaosMode::Swarm), Chaos::BuggifyKnobs])
        .chaos_duration(Duration::from_secs(10))
}
