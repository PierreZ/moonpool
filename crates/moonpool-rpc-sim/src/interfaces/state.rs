//! The campaign's independent records, kept in the simulation's
//! `StateHandle` and written outside the transport:
//!
//! - **boots**: how many times each participant booted (its current boot is
//!   the last one), bumped by the process before it touches the network;
//! - **publications**: every interface a boot published, keyed by the
//!   reference bytes, with the participant, boot and configuration that
//!   published it, recorded by the publisher when it registers;
//! - **executions**: every handler execution, keyed by probe id, with the
//!   participant, boot and configuration that ran it.
//!
//! The workload judges its outcomes against these, never against the RPC
//! runtime's registry or queues.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_sim::StateHandle;

const LEDGER_KEY: &str = "rpc.interfaces.ledger";
/// Board label of the workload's runtime.
pub const WORKLOAD_LABEL: &str = "workload";
/// Set once the fault script stopped rebooting participants.
pub const SCRIPT_DONE_KEY: &str = "rpc.interfaces.script.done";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("Mutex poisoned: prior task panicked")
}

/// Who published or ran something: application identity, not RPC identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instance {
    /// The participant's IP.
    pub participant: String,
    /// Its boot number.
    pub boot: u64,
    /// The configuration (0: the base instance).
    pub configuration: u64,
}

#[derive(Default)]
struct LedgerState {
    boots: BTreeMap<String, u64>,
    publications: BTreeMap<Vec<u8>, Instance>,
    executions: BTreeMap<u64, Vec<Instance>>,
    restarts: u64,
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

    /// A participant booted; returns its new boot number.
    #[must_use]
    pub fn boot(&self, participant: &str) -> u64 {
        let mut inner = lock(&self.inner);
        let boot = inner.boots.entry(participant.to_string()).or_insert(0);
        *boot += 1;
        let boot = *boot;
        if boot > 1 {
            inner.restarts += 1;
        }
        boot
    }

    /// The participant's current boot (0 before its first).
    #[must_use]
    pub fn current_boot(&self, participant: &str) -> u64 {
        lock(&self.inner)
            .boots
            .get(participant)
            .copied()
            .unwrap_or(0)
    }

    /// Restarts observed so far, over every participant.
    #[must_use]
    pub fn restarts(&self) -> u64 {
        lock(&self.inner).restarts
    }

    /// Record that `instance` published the reference `bytes`.
    pub fn publish(&self, bytes: Vec<u8>, instance: Instance) {
        lock(&self.inner).publications.insert(bytes, instance);
    }

    /// Who published the reference `bytes`, if anyone did.
    #[must_use]
    pub fn publisher(&self, bytes: &[u8]) -> Option<Instance> {
        lock(&self.inner).publications.get(bytes).cloned()
    }

    /// Record that `instance` ran probe `id`; returns how many times it ran.
    #[must_use]
    pub fn execute(&self, id: u64, instance: Instance) -> usize {
        let mut inner = lock(&self.inner);
        let runs = inner.executions.entry(id).or_default();
        runs.push(instance);
        runs.len()
    }

    /// Every execution of probe `id`.
    #[must_use]
    pub fn executions(&self, id: u64) -> Vec<Instance> {
        lock(&self.inner)
            .executions
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }
}
