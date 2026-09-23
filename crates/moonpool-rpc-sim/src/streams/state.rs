//! The campaign's shared state and its two external ledgers.
//!
//! The **producer ledger** is written by the producer's handler code: which
//! boot served a stream, every item it queued (with its accounted size) and
//! how its producer ended. The **consumer ledger** is written by the
//! workload: every item its application took, in order, and the bytes that
//! consumption released. The workload judges each stream by comparing the
//! two; nothing reads the RPC runtime's queues or credit counters to decide
//! what should have happened.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use moonpool_sim::StateHandle;

const LEDGER_KEY: &str = "rpc.streams.ledger";
/// How many times the producer booted.
pub const PRODUCER_BOOTS_KEY: &str = "rpc.streams.producer.boots";
/// Reboot requests from the workload: crash (`false`) or graceful (`true`).
pub const REBOOT_REQUESTS_KEY: &str = "rpc.streams.reboots";
/// Set once the fault script ended (chaos is over).
pub const FAULTS_DONE_KEY: &str = "rpc.streams.faults.done";
/// Board label of the workload's runtime.
pub const WORKLOAD_LABEL: &str = "workload";
/// Board label prefix of the producer's runtimes (`producer#<boot>`).
pub const PRODUCER_LABEL: &str = "producer#";

/// How a producer's stream ended, as its handler saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerEnd {
    /// It ended the stream normally.
    Finished,
    /// It failed the stream with this code.
    Failed(u64),
    /// It dropped its producer: a broken promise.
    Dropped,
    /// Its item was refused as larger than the window, then it dropped the
    /// producer.
    TooLarge,
    /// A send failed: the consumer abandoned the stream.
    Cancelled,
    /// A send failed: the connection ended.
    Disconnected,
    /// A send failed for another reason (shutdown, protocol).
    Other,
}

/// One stream as its producer recorded it.
#[derive(Debug, Clone, Default)]
pub struct Produced {
    /// The producer boot that served it.
    pub boot: u64,
    /// The window its producer was held to.
    pub window: u64,
    /// The accounted size of every item it queued, in order.
    pub items: Vec<u64>,
    /// Cumulative accounted bytes queued.
    pub sent_bytes: u64,
    /// How it ended, once it did.
    pub end: Option<ProducerEnd>,
    /// A send had to wait and completed after consumption resumed.
    pub waited: bool,
    /// Accounted bytes sent and unconsumed when the producer ended it.
    pub in_flight_at_end: u64,
}

/// One stream as the workload's application consumed it.
#[derive(Debug, Clone, Default)]
pub struct Consumed {
    /// `(sequence, accounted size, producer boot)` of every item taken.
    pub items: Vec<(u64, u64, u64)>,
    /// Cumulative accounted bytes taken.
    pub bytes: u64,
}

#[derive(Default)]
struct LedgerState {
    produced: BTreeMap<u64, Produced>,
    consumed: BTreeMap<u64, Consumed>,
    probes: BTreeMap<u64, u32>,
}

/// The run's producer and consumer ledgers.
#[derive(Clone, Default)]
pub struct StreamLedger {
    inner: Arc<Mutex<LedgerState>>,
}

impl StreamLedger {
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

    fn lock(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.inner
            .lock()
            .expect("Mutex poisoned: prior task panicked")
    }

    /// A producer took stream `id` in `boot`.
    pub fn open(&self, id: u64, boot: u64, window: u64) {
        self.lock().produced.insert(
            id,
            Produced {
                boot,
                window,
                ..Produced::default()
            },
        );
    }

    /// The producer queued an item of `size` bytes; returns the bytes queued
    /// so far and the bytes the consumer has taken.
    #[must_use]
    pub fn sent(&self, id: u64, size: u64, waited: bool) -> (u64, u64) {
        let mut state = self.lock();
        let consumed = state.consumed.get(&id).map_or(0, |entry| entry.bytes);
        let entry = state.produced.entry(id).or_default();
        entry.items.push(size);
        entry.sent_bytes += size;
        entry.waited |= waited;
        (entry.sent_bytes, consumed)
    }

    /// The producer of stream `id` ended it.
    pub fn ended(&self, id: u64, end: ProducerEnd, in_flight: u64) {
        let mut state = self.lock();
        let entry = state.produced.entry(id).or_default();
        if entry.end.is_none() {
            entry.end = Some(end);
            entry.in_flight_at_end = in_flight;
        }
    }

    /// Stream `id` as its producer recorded it.
    #[must_use]
    pub fn produced(&self, id: u64) -> Option<Produced> {
        self.lock().produced.get(&id).cloned()
    }

    /// Every stream a producer recorded.
    #[must_use]
    pub fn all_produced(&self) -> BTreeMap<u64, Produced> {
        self.lock().produced.clone()
    }

    /// The workload's application took item `seq` of `size` bytes, sent by
    /// `boot`.
    pub fn consume(&self, id: u64, seq: u64, size: u64, boot: u64) {
        let mut state = self.lock();
        let entry = state.consumed.entry(id).or_default();
        entry.items.push((seq, size, boot));
        entry.bytes += size;
    }

    /// Stream `id` as the workload consumed it.
    #[must_use]
    pub fn consumed(&self, id: u64) -> Consumed {
        self.lock().consumed.get(&id).cloned().unwrap_or_default()
    }

    /// A probe handler ran for `id`.
    pub fn probe(&self, id: u64) {
        *self.lock().probes.entry(id).or_insert(0) += 1;
    }

    /// How many times a probe handler ran for `id`.
    #[must_use]
    pub fn probes(&self, id: u64) -> u32 {
        self.lock().probes.get(&id).copied().unwrap_or(0)
    }
}
