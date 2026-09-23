//! The campaign's independent records, kept in the simulation's
//! `StateHandle` and written only by application code, never by reading
//! the transport:
//!
//! - **boots**: each process bumps its boot before it touches the network;
//! - **publications**: every reference a boot published, keyed by its
//!   bytes, with the process, boot and configuration that published it,
//!   written by the publisher when it registers (before advertising it);
//! - **executions**: every handler run, keyed by job id, with the instance
//!   that ran it, written when user code first receives the request.
//!
//! Streams are judged by the streams campaign's producer/consumer ledger
//! ([`crate::streams::state::StreamLedger`]) and credentials by the
//! security campaign's issue/receipt ledger
//! ([`crate::security::state::Ledger`]); balanced calls by the executions
//! their policy allows (the observation hook, the balancer's own account,
//! only tightens that bound).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_sim::StateHandle;

const LEDGER_KEY: &str = "rpc.qual.ledger";
/// Set once the fault script stopped (clock restored, processes back).
pub const SCRIPT_DONE_KEY: &str = "rpc.qual.script.done";
/// The workload lanes' IPs, as `Vec<String>`, for scripted cuts.
pub const WORKLOAD_IP_KEY: &str = "rpc.qual.workload.ips";
/// Cuts a lane asked for, as `Vec<(lane IP, server IP)>` (appended; the
/// script serves each once).
pub const CUT_REQUESTS_KEY: &str = "rpc.qual.cuts";
/// Servers a lane asked to be shut down gracefully, as `Vec<String>`
/// (appended; the script serves each once, during the chaos window).
pub const SHUTDOWN_REQUESTS_KEY: &str = "rpc.qual.shutdowns";
/// How many lanes stopped issuing work and drained their balancers.
pub const LANES_STOPPED_KEY: &str = "rpc.qual.lanes.stopped";
/// Set by the lead lane once the idle baseline was checked: every lane
/// may judge its calls.
pub const QUIESCED_KEY: &str = "rpc.qual.quiesced";
/// Board label prefix of the lanes' main runtimes.
pub const WORKLOAD_LABEL: &str = "qual-workload";
/// Board label prefix of the lanes' version 1 runtimes.
pub const VERSION1_LABEL: &str = "qual-workload-v1";

/// Where a lane leaves its history for the lead lane's record.
#[must_use]
pub fn lane_key(lane: usize) -> String {
    format!("rpc.qual.lane.{lane}")
}

/// What a lane leaves behind at the end of its run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneRecord {
    /// Its history.
    pub history: Vec<String>,
    /// Its runtimes returned to baseline once their drivers dropped.
    pub baseline: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("Mutex poisoned: prior task panicked")
}

/// Who published or ran something: application identity, not RPC identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instance {
    /// The process (its IP).
    pub process: String,
    /// Its boot.
    pub boot: u64,
    /// The configuration (0: a base instance).
    pub configuration: u64,
}

impl std::fmt::Display for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}/{}", self.process, self.boot, self.configuration)
    }
}

/// A process and boot as one number, for the streams ledger's boot field:
/// the IP's last octet above the boot.
#[must_use]
pub fn instance_code(process: &str, boot: u64) -> u64 {
    let octet = process
        .rsplit('.')
        .next()
        .and_then(|octet| octet.parse::<u64>().ok())
        .unwrap_or(0);
    let group = process
        .split('.')
        .nth(2)
        .and_then(|octet| octet.parse::<u64>().ok())
        .unwrap_or(0);
    (group << 48) | (octet << 32) | boot
}

#[derive(Default)]
struct LedgerState {
    boots: BTreeMap<String, u64>,
    publications: BTreeMap<Vec<u8>, Instance>,
    executions: BTreeMap<u64, Vec<Instance>>,
    /// Graceful shutdowns under way: process → boot.
    draining: BTreeMap<String, u64>,
    /// Each boot's base `Work` reference.
    base: BTreeMap<(String, u64), Vec<u8>>,
    /// Processes the script took down and that have not booted since.
    down: std::collections::BTreeSet<String>,
}

/// The run's records. Clones share them.
#[derive(Clone, Default)]
pub struct QualLedger {
    inner: Arc<Mutex<LedgerState>>,
}

impl QualLedger {
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

    /// A process booted; returns its new boot number.
    #[must_use]
    pub fn boot(&self, process: &str) -> u64 {
        let mut inner = lock(&self.inner);
        inner.draining.remove(process);
        inner.down.remove(process);
        let boot = inner.boots.entry(process.to_string()).or_insert(0);
        *boot += 1;
        *boot
    }

    /// The script took `process` down (it boots again later).
    pub fn down(&self, process: &str) {
        lock(&self.inner).down.insert(process.to_string());
    }

    /// Whether `process`'s boot `boot` has ended: a later boot exists, or
    /// the script took the process down since.
    #[must_use]
    pub fn ended(&self, process: &str, boot: u64) -> bool {
        let inner = lock(&self.inner);
        inner.boots.get(process).copied().unwrap_or(0) > boot || inner.down.contains(process)
    }

    /// The process's current boot (0 before its first).
    #[must_use]
    pub fn current_boot(&self, process: &str) -> u64 {
        lock(&self.inner).boots.get(process).copied().unwrap_or(0)
    }

    /// Record that `instance` published the reference `bytes`.
    pub fn publish(&self, bytes: Vec<u8>, instance: Instance) {
        lock(&self.inner).publications.insert(bytes, instance);
    }

    /// Record a boot's base `Work` reference (also a publication).
    pub fn publish_base(&self, bytes: Vec<u8>, instance: Instance) {
        let mut inner = lock(&self.inner);
        inner
            .base
            .insert((instance.process.clone(), instance.boot), bytes.clone());
        inner.publications.insert(bytes, instance);
    }

    /// The base `Work` reference boot `boot` of `process` published.
    #[must_use]
    pub fn base_work(&self, process: &str, boot: u64) -> Option<Vec<u8>> {
        lock(&self.inner)
            .base
            .get(&(process.to_string(), boot))
            .cloned()
    }

    /// Who published the reference `bytes`, if anyone did.
    #[must_use]
    pub fn publisher(&self, bytes: &[u8]) -> Option<Instance> {
        lock(&self.inner).publications.get(bytes).cloned()
    }

    /// Record that `instance` ran job `id`; returns how many times it ran.
    #[must_use]
    pub fn execute(&self, id: u64, instance: Instance) -> usize {
        let mut inner = lock(&self.inner);
        let runs = inner.executions.entry(id).or_default();
        runs.push(instance);
        runs.len()
    }

    /// Every execution of job `id`.
    #[must_use]
    pub fn executions(&self, id: u64) -> Vec<Instance> {
        lock(&self.inner)
            .executions
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }

    /// A process's boot began a graceful shutdown.
    pub fn draining(&self, process: &str, boot: u64) {
        lock(&self.inner).draining.insert(process.to_string(), boot);
    }

    /// Processes draining right now, with their boots.
    #[must_use]
    pub fn drains(&self) -> BTreeMap<String, u64> {
        lock(&self.inner).draining.clone()
    }

    /// The ledger as sorted lines: boots, publications and executions.
    /// Two runs of one seed must produce the same digest.
    #[must_use]
    pub fn digest(&self) -> Vec<String> {
        let inner = lock(&self.inner);
        let mut lines = Vec::new();
        for (process, boot) in &inner.boots {
            lines.push(format!("boots {process} {boot}"));
        }
        let mut publications: Vec<String> = inner
            .publications
            .values()
            .map(|instance| format!("published {instance}"))
            .collect();
        publications.sort();
        lines.extend(publications);
        for (id, runs) in &inner.executions {
            let runs: Vec<String> = runs.iter().map(ToString::to_string).collect();
            lines.push(format!("executed {id} {}", runs.join(" ")));
        }
        lines
    }
}
