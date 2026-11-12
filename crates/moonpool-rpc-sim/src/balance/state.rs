//! The campaign's independent records, kept in the simulation's
//! `StateHandle` and written outside the balancer and the transport:
//!
//! - **receipts**: every handler run, keyed by job id, with the server,
//!   boot, whether it declined and the value it computed, written by the
//!   handler before it answers;
//! - **boots** and **publications**: each server bumps its boot before it
//!   touches the network and publishes its current reference (the
//!   directory the workload learns from, explicitly);
//! - **outages**: the fault script records when every alternative is down
//!   and when it restored them.
//!
//! The workload judges its outcomes against these and against its own
//! attempt ledger (fed by the balancer's observation hook), never against
//! the queue model's bookkeeping.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_sim::StateHandle;

use crate::lock;

const LEDGER_KEY: &str = "rpc.balance.ledger";
/// Board label of the workload's runtime.
pub const WORKLOAD_LABEL: &str = "balance-workload";
/// The workload's IP, published for the fault script.
pub const WORKLOAD_IP_KEY: &str = "rpc.balance.workload.ip";
/// Set once the fault script stopped injecting.
pub const SCRIPT_DONE_KEY: &str = "rpc.balance.script.done";

/// One handler run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The server's IP.
    pub server: String,
    /// Its boot.
    pub boot: u64,
    /// It declined the job (temporarily behind): no effect.
    pub declined: bool,
    /// The value it computed.
    pub value: u64,
}

/// A server's current reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// The boot that registered it.
    pub boot: u64,
    /// Registrations within the boot (a destroyed endpoint is replaced).
    pub generation: u64,
    /// The encoded `ServiceRef`.
    pub reference: Vec<u8>,
}

#[derive(Default)]
struct LedgerState {
    receipts: BTreeMap<u64, Vec<Receipt>>,
    boots: BTreeMap<String, u64>,
    publications: BTreeMap<String, Publication>,
    outage: bool,
    outages: u64,
}

/// The run's records.
#[derive(Clone, Default)]
pub struct Ledger {
    inner: Arc<Mutex<LedgerState>>,
}

impl Ledger {
    /// The run's ledger, created on first use.
    #[must_use]
    pub fn of(state: &StateHandle) -> Self {
        if let Some(ledger) = state.get::<Self>(LEDGER_KEY) {
            return ledger;
        }
        let ledger = Self::default();
        state.publish(LEDGER_KEY, ledger.clone());
        ledger
    }

    /// Start a new boot of `server`; returns its number.
    #[must_use]
    pub fn boot(&self, server: &str) -> u64 {
        let mut inner = lock(&self.inner);
        let boot = inner.boots.entry(server.to_string()).or_insert(0);
        *boot += 1;
        *boot
    }

    /// The current boot of `server`.
    #[must_use]
    pub fn current_boot(&self, server: &str) -> u64 {
        lock(&self.inner).boots.get(server).copied().unwrap_or(0)
    }

    /// Record a handler run of job `id`.
    pub fn receive(&self, id: u64, receipt: Receipt) {
        lock(&self.inner)
            .receipts
            .entry(id)
            .or_default()
            .push(receipt);
    }

    /// Every handler run of job `id`.
    #[must_use]
    pub fn receipts(&self, id: u64) -> Vec<Receipt> {
        lock(&self.inner)
            .receipts
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    /// Publish `server`'s current reference.
    pub fn publish(&self, server: &str, publication: Publication) {
        lock(&self.inner)
            .publications
            .insert(server.to_string(), publication);
    }

    /// Every server's latest publication.
    #[must_use]
    pub fn directory(&self) -> BTreeMap<String, Publication> {
        lock(&self.inner).publications.clone()
    }

    /// Every alternative is down (`true`) or was restored (`false`).
    pub fn set_outage(&self, down: bool) {
        let mut inner = lock(&self.inner);
        if inner.outage && !down {
            inner.outages += 1;
        }
        inner.outage = down;
    }

    /// Whether every alternative is down right now.
    #[must_use]
    pub fn outage(&self) -> bool {
        lock(&self.inner).outage
    }

    /// Outages that ended.
    #[must_use]
    pub fn outages(&self) -> u64 {
        lock(&self.inner).outages
    }

    /// The outage state now, to compare with [`Ledger::outage_since`].
    #[must_use]
    pub fn outage_mark(&self) -> OutageMark {
        let inner = lock(&self.inner);
        OutageMark {
            down: inner.outage,
            ended: inner.outages,
        }
    }

    /// Whether every alternative was down at some time since `mark`: at
    /// the mark, now, or in an outage that started and ended between.
    #[must_use]
    pub fn outage_since(&self, mark: OutageMark) -> bool {
        let inner = lock(&self.inner);
        mark.down || inner.outage || inner.outages != mark.ended
    }
}

/// The outage state at one time (see [`Ledger::outage_mark`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutageMark {
    down: bool,
    ended: u64,
}

#[cfg(test)]
mod tests {
    use super::Ledger;

    #[test]
    fn an_outage_inside_the_interval_is_seen() {
        let ledger = Ledger::default();
        let mark = ledger.outage_mark();
        assert!(!ledger.outage_since(mark));
        // Starts and ends between the two reads: the flag alone is false
        // at both ends.
        ledger.set_outage(true);
        ledger.set_outage(false);
        assert!(!ledger.outage());
        assert!(ledger.outage_since(mark));
        assert!(!ledger.outage_since(ledger.outage_mark()));
    }

    #[test]
    fn an_outage_open_at_either_end_is_seen() {
        let ledger = Ledger::default();
        ledger.set_outage(true);
        let mark = ledger.outage_mark();
        assert!(ledger.outage_since(mark));
        let ledger = Ledger::default();
        let mark = ledger.outage_mark();
        ledger.set_outage(true);
        assert!(ledger.outage_since(mark));
    }
}
