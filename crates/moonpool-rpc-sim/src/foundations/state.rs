//! Per-run shared state, kept in the simulation's `StateHandle`.
//!
//! Everything here is *outside* the transport: the receipt ledger is written
//! by handler code the moment a request reaches user code, and the workload
//! judges its own outcomes against it. Nothing reads the RPC runtime's queues
//! or registry to decide what should have happened.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_rpc::{ResourceProbe, RpcStats};
use moonpool_sim::StateHandle;

const LEDGER_KEY: &str = "rpc.ledger";
const BOARD_KEY: &str = "rpc.board";
/// Latest server references, as [`ServerRefs`].
pub const SERVER_REFS_KEY: &str = "rpc.server.refs";
/// Latest ephemeral endpoint reference bytes, as `(sequence, bytes)`.
pub const EPHEMERAL_KEY: &str = "rpc.server.ephemeral";
/// The relay's reference bytes.
pub const RELAY_REF_KEY: &str = "rpc.relay.ref";
/// How many crash-after-receipt requests the server has received.
pub const CRASH_REQUESTS_KEY: &str = "rpc.crash.requests";
/// How many times the server process has booted.
pub const SERVER_BOOTS_KEY: &str = "rpc.server.boots";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().expect("Mutex poisoned: prior task panicked")
}

/// Handler receipts per workload request id.
#[derive(Clone, Default)]
pub struct Ledger {
    receipts: Arc<Mutex<BTreeMap<u64, u32>>>,
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

    /// Record that a handler received request `id`; returns the new count.
    #[must_use]
    pub fn record(&self, id: u64) -> u32 {
        let mut receipts = lock(&self.receipts);
        let count = receipts.entry(id).or_insert(0);
        *count += 1;
        *count
    }

    /// How many times a handler received request `id`.
    #[must_use]
    pub fn receipts(&self, id: u64) -> u32 {
        lock(&self.receipts).get(&id).copied().unwrap_or(0)
    }
}

/// Serialised references a server boot published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRefs {
    /// The boot that registered them.
    pub boot: u64,
    /// [`Echo`](super::messages::Echo).
    pub echo: Vec<u8>,
    /// [`Slow`](super::messages::Slow).
    pub slow: Vec<u8>,
    /// [`CrashAfterReceipt`](super::messages::CrashAfterReceipt).
    pub crash: Vec<u8>,
}

/// Every runtime's latest counters and its drop probe, by runtime label.
#[derive(Clone, Default)]
pub struct Board {
    inner: Arc<Mutex<BoardState>>,
}

#[derive(Default)]
struct BoardState {
    stats: BTreeMap<String, RpcStats>,
    probes: BTreeMap<String, ResourceProbe>,
}

impl Board {
    /// The run's board, created on first use.
    #[must_use]
    pub fn of(state: &StateHandle) -> Self {
        if let Some(board) = state.get::<Self>(BOARD_KEY) {
            return board;
        }
        let board = Self::default();
        state.publish(BOARD_KEY, board.clone());
        board
    }

    /// Register a runtime's drop probe under `label`.
    pub fn register_probe(&self, label: &str, probe: ResourceProbe) {
        lock(&self.inner).probes.insert(label.to_string(), probe);
    }

    /// Record a runtime's latest counters under `label`.
    pub fn report(&self, label: &str, stats: RpcStats) {
        lock(&self.inner).stats.insert(label.to_string(), stats);
    }

    /// Every probe whose label starts with `prefix`, except `except`.
    #[must_use]
    pub fn probes(&self, prefix: &str, except: &str) -> Vec<(String, ResourceProbe)> {
        lock(&self.inner)
            .probes
            .iter()
            .filter(|(label, _)| label.starts_with(prefix) && label.as_str() != except)
            .map(|(label, probe)| (label.clone(), probe.clone()))
            .collect()
    }

    /// Counters summed over every runtime ever reported.
    #[must_use]
    pub fn totals(&self) -> RpcStats {
        let inner = lock(&self.inner);
        let mut total = RpcStats::default();
        for stats in inner.stats.values() {
            total.protocol_violations += stats.protocol_violations;
            total.checksum_failures += stats.checksum_failures;
            total.established_checksum_failures += stats.established_checksum_failures;
            total.version_rejections += stats.version_rejections;
            total.late_replies += stats.late_replies;
            total.calls_abandoned += stats.calls_abandoned;
            total.requests_admitted += stats.requests_admitted;
            total.requests_rejected += stats.requests_rejected;
            total.replies_dropped += stats.replies_dropped;
            total.misrouted_replies += stats.misrouted_replies;
        }
        total
    }
}

/// Bump and return a counter kept in the state.
#[must_use]
pub fn bump(state: &StateHandle, key: &str) -> u64 {
    let next = state.get::<u64>(key).unwrap_or(0) + 1;
    state.publish(key, next);
    next
}
