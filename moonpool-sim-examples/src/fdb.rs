//! A fake FoundationDB — the entire dependency in one file.
//!
//! This module shows how a layer author can ship a faithful in-memory FDB
//! alongside the production binding and **never start a Docker container in
//! their test suite again**.
//!
//! What's here:
//! - [`FdbDatabase`] / [`FdbTransaction`] — the trait boundary the layer
//!   targets. A `RealFdbDatabase` calling the C bindings would implement
//!   these alongside [`MockDatabase`]; layer code never knows the difference.
//! - [`MockDatabase`] — an in-memory implementation with real MVCC: per-key
//!   version chains, snapshot reads, commit-time conflict detection.
//! - Version generation lifted from FDB's master (`version += max(1,
//!   elapsed_µs)`, no ceiling), anchored to an `Instant` the fake owns.
//! - [`MockConfig`] knobs to inject the three layer-author headaches —
//!   [`FdbError::NotCommitted`], [`FdbError::CommitUnknownResult`],
//!   [`FdbError::TransactionTooOld`] — at configurable rates, plus per-op
//!   latency. A real FDB container cannot produce these on demand.
//!
//! What's deliberately not here: `get_range`, atomic ops, snapshot reads,
//! versionstamps, the `on_error` retry helper. Add them if your layer needs
//! them — each is a few more lines of `BTreeMap::range` or buffer-flag logic.
//! This file is the headline: a faithful FDB fake in under 400 lines.
//!
//! # Why this is so cheap
//!
//! Writing a fake feels expensive until you tally what it actually costs:
//!
//! - **Cluster state**: four fields on a struct. No replication, no storage
//!   servers, no proxies — those exist to scale, and a fake doesn't scale.
//! - **MVCC**: a `Vec<(Version, Option<Value>)>` per key. Snapshot reads
//!   walk it backwards; that's the whole snapshot-isolation engine.
//! - **The resolver**: one `for` loop in `commit`. Real FDB ships an entire
//!   cluster role for this; we do it inline because there's no network
//!   between proxies and resolvers when "the cluster" is a `Mutex`.
//! - **Multi-client topology**: `Arc::clone`. No discovery, no coordinators,
//!   no `fdb.cluster` file — every derived `MockDatabase` already sees the
//!   same data because they share an `Arc`.
//! - **Partial failures**: `if random_bool(prob) { return Err(...) }`. A real
//!   FDB testcontainer would need network partitions, killed proxies, and
//!   precise timing to produce a `CommitUnknownResult` on demand. We do it
//!   in three lines, deterministically, from a seed.
//!
//! # Why we lean on `Providers`
//!
//! Time and randomness arrive through a [`Providers`] bundle, never through
//! `std::time::Instant` or `tokio::time::sleep` directly. That single choice
//! is what lets this fake work in **both** worlds:
//!
//! - With [`TokioProviders`]: it's a normal in-memory fake for `#[tokio::test]`
//!   — wall-clock sleeps, thread-local RNG.
//! - With `SimProviders` (from moonpool-sim): every `time().sleep()` advances
//!   simulation time, every `random()` call comes from the seeded simulation
//!   RNG. Two runs with the same seed produce byte-identical histories,
//!   including the exact sequence of `NotCommitted` / `CommitUnknownResult`
//!   / `TransactionTooOld` injections.
//!
//! A fake hard-wired to `tokio::time::sleep` cannot do this — its latencies
//! and chaos draws are non-deterministic even under a seed, breaking
//! moonpool's reproducibility guarantee. The provider indirection is what
//! turns the fake from "an in-memory database" into "an in-memory database
//! that's also a moonpool citizen".

#![allow(clippy::missing_errors_doc, clippy::doc_markdown)]

use std::collections::BTreeMap;
use std::ops::{Bound, Range};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;

use moonpool_sim::{Providers, RandomProvider, TimeProvider};

// ============================================================================
// Domain types
// ============================================================================

/// FDB stores arbitrary byte strings as keys.
pub type Key = Bytes;
/// FDB stores arbitrary byte strings as values.
pub type Value = Bytes;
/// A 64-bit monotonic version assigned at commit time by the cluster.
pub type Version = u64;

// ============================================================================
// Errors — the three a layer author actually has to think about
// ============================================================================

/// Errors a transaction can return. Mirrors the canonical FDB error codes
/// every layer must handle.
///
/// These three are partial failures: the cluster as a whole is fine, but
/// *this* operation didn't go through cleanly. A real FDB testcontainer
/// can crash the container (binary up/down), but cannot make a single
/// commit return `CommitUnknownResult` on demand. The fake produces all
/// three at any rate you ask for, reproducibly from a seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FdbError {
    /// Read-write conflict detected at commit. Another transaction wrote
    /// a key in this transaction's read conflict range between its
    /// `read_version` and its attempted `commit_version`. Safe to retry.
    #[error("not_committed: read-write conflict detected at commit")]
    NotCommitted,

    /// Commit status is unknown. The mutations may or may not have been
    /// applied to the cluster — the client cannot tell. Retrying is only
    /// safe if the transaction is idempotent; otherwise re-read first to
    /// determine whether the commit landed.
    #[error("commit_unknown_result: status of commit is unknown")]
    CommitUnknownResult,

    /// Transaction has outlived the cluster's read-transaction window
    /// (5 seconds in real FDB). Start a fresh transaction.
    #[error("transaction_too_old: read version expired")]
    TransactionTooOld,
}

// ============================================================================
// The trait boundary — what application code depends on
// ============================================================================

/// Handle to an FDB cluster. Cheaply cloneable — clones share the underlying
/// cluster handle, just like real FDB's `Database` handles. Application code
/// is generic over `D: FdbDatabase` so it can be tested against `MockDatabase`
/// and shipped against `RealFdbDatabase`.
///
/// **This trait is the entire prod/sim boundary.** Production code never
/// names `MockDatabase` and tests never name `RealFdbDatabase`. Swap the
/// type at the call site and the layer doesn't notice.
pub trait FdbDatabase: Clone + Send + Sync + 'static {
    /// The transaction type produced by this database.
    type Transaction: FdbTransaction;

    /// Begin a new transaction. The cluster's current version is captured as
    /// the transaction's snapshot `read_version` — all reads see state as of
    /// this version even if other transactions commit afterwards.
    fn create_transaction(&self) -> Self::Transaction;
}

/// A transaction. Reads are snapshot-isolated to the transaction's
/// `read_version`; writes are buffered locally and applied atomically at
/// commit time. Commit fails with [`FdbError::NotCommitted`] if any key
/// covered by a read conflict range has been written since `read_version`.
pub trait FdbTransaction: Send {
    /// Read a key. Returns `Ok(None)` if absent at the snapshot version.
    /// Adds an implicit read conflict range covering `key` exactly.
    async fn get(&mut self, key: Key) -> Result<Option<Value>, FdbError>;

    /// Buffer a write. Applied on `commit`.
    fn put(&mut self, key: Key, value: Value);

    /// Buffer a delete. Applied on `commit`.
    fn delete(&mut self, key: Key);

    /// Apply all buffered mutations atomically at a new `commit_version`.
    /// Returns the assigned commit version on success.
    async fn commit(self) -> Result<Version, FdbError>;
}

// ============================================================================
// MockConfig — how chaotic should the fake be?
// ============================================================================

/// Configuration knobs for [`MockDatabase`]: per-op latency ranges and
/// error-injection probabilities. Randomness comes from the bundled
/// [`Providers`], not from a seed stored here.
#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Range of latencies (milliseconds) applied to each `commit`.
    pub commit_latency_ms: Range<u64>,
    /// Range of latencies (milliseconds) applied to each `get`.
    pub get_latency_ms: Range<u64>,
    /// Probability per `commit` of injecting a synthetic `NotCommitted`
    /// before any mutations are applied.
    pub not_committed_prob: f64,
    /// Probability per `commit` of injecting `CommitUnknownResult` **after**
    /// mutations have been applied — exactly mirroring real FDB's most
    /// dangerous failure mode.
    pub commit_unknown_result_prob: f64,
    /// Probability per `get` of injecting `TransactionTooOld`.
    pub transaction_too_old_prob: f64,
}

impl MockConfig {
    /// No latency, no injected errors. Use this when unit-testing the layer's
    /// MVCC semantics and you don't want chaos noise.
    #[must_use]
    pub fn deterministic() -> Self {
        Self {
            commit_latency_ms: 0..1,
            get_latency_ms: 0..1,
            not_committed_prob: 0.0,
            commit_unknown_result_prob: 0.0,
            transaction_too_old_prob: 0.0,
        }
    }

    /// Realistic latency and a steady stream of partial failures. Use this
    /// when stress-testing the layer's retry and idempotency logic.
    #[must_use]
    pub fn chaos() -> Self {
        Self {
            commit_latency_ms: 1..20,
            get_latency_ms: 0..4,
            not_committed_prob: 0.05,
            commit_unknown_result_prob: 0.05,
            transaction_too_old_prob: 0.02,
        }
    }
}

// ============================================================================
// MockFdb — the shared cluster state
// ============================================================================

/// The in-memory cluster. Wrapped in `Arc<Mutex<…>>` and shared by every
/// derived [`MockDatabase`] handle — the same way every real FDB client in a
/// cluster talks to the same masters and storage servers.
///
/// Look at the fields: `data`, `version`, `last_anchor_us`, `config`. Four
/// fields. That's the entire cluster. (Time and randomness live on the
/// [`Providers`] bundle that wraps the cluster — they're inputs, not state.)
#[doc(hidden)]
pub struct MockFdb {
    /// Per-key version chain. `BTreeMap` because conflict detection walks
    /// key ranges. Inner `Vec` is append-only, sorted by version, and stores
    /// `None` for tombstones (deletes).
    data: BTreeMap<Key, Vec<(Version, Option<Value>)>>,

    /// Monotonic version counter. Advanced by `next_commit_version`.
    version: Version,

    /// `time().now()` in µs at the previous `next_commit_version` call. Each
    /// call advances `version` by `max(1, now_us - last_anchor_us)`.
    last_anchor_us: u64,

    config: MockConfig,
}

impl MockFdb {
    /// FDB master's version-generation algorithm, ceiling removed.
    ///
    /// Real FDB: `version += clamp(elapsed_µs, 1, MAX_READ_TXN_LIFE_VERSIONS)`.
    /// The floor of 1 guarantees strict monotonicity even for two requests
    /// in the same microsecond; the ceiling caps how far an idle gap can
    /// jump the clock so in-flight reads don't all instantly become
    /// `transaction_too_old`.
    ///
    /// We drop the ceiling on purpose. The caller can still observe
    /// `TransactionTooOld` via `MockConfig::transaction_too_old_prob`, and
    /// removing the clamp keeps the function trivially auditable.
    ///
    /// `time` is taken via [`TimeProvider`], not `Instant::now`, so the
    /// version sequence is deterministic under `SimProviders`.
    fn next_commit_version<T: TimeProvider>(&mut self, time: &T) -> Version {
        let now_us = u64::try_from(time.now().as_micros()).unwrap_or(u64::MAX);
        let to_add = now_us.saturating_sub(self.last_anchor_us).max(1);
        self.version = self.version.saturating_add(to_add);
        self.last_anchor_us = now_us;
        self.version
    }

    /// Walk the version chain backwards and return the value at the most
    /// recent version `<= at`. `None` for absent keys *or* tombstones.
    fn read_at(&self, key: &Key, at: Version) -> Option<Value> {
        self.data
            .get(key)?
            .iter()
            .rev()
            .find(|(v, _)| *v <= at)
            .and_then(|(_, val)| val.clone())
    }

    /// Conflict-detection primitive: was any key in the half-open range
    /// `[begin, end)` written at a version strictly greater than `since`?
    fn any_write_in_range_since(&self, begin: &Key, end: &Key, since: Version) -> bool {
        self.data
            .range::<Key, _>((Bound::Included(begin.clone()), Bound::Excluded(end.clone())))
            .any(|(_, chain)| chain.iter().any(|(v, _)| *v > since))
    }
}

// ============================================================================
// MockDatabase — implements FdbDatabase
// ============================================================================

/// In-memory FDB. Hand out as many clones as you like; they all share the
/// same backing cluster state.
///
/// Generic over `P: Providers` so the same fake plugs into both production
/// tests (`TokioProviders` → wall-clock sleeps, thread-local RNG) and
/// moonpool simulations (`SimProviders` → simulated time, seeded RNG, fully
/// deterministic).
///
/// Compare the setup cost to a real FDB testcontainer: `MockDatabase::new`
/// is a few allocations; spinning up `foundationdb` in Docker takes seconds,
/// half a gigabyte of RAM, and a `wait_for` loop. Twenty parallel test
/// cases? Twenty `MockDatabase::new` calls versus twenty containers.
#[derive(Clone)]
pub struct MockDatabase<P: Providers> {
    inner: Arc<Mutex<MockFdb>>,
    providers: P,
}

impl<P: Providers> MockDatabase<P> {
    /// Create a fresh, empty cluster with the given configuration and
    /// provider bundle. All randomness (chaos, latency) is drawn from
    /// `providers.random()`; all sleeps go through `providers.time()`.
    #[must_use]
    pub fn new(providers: P, config: MockConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockFdb {
                data: BTreeMap::new(),
                version: 0,
                last_anchor_us: 0,
                config,
            })),
            providers,
        }
    }
}

impl<P: Providers> FdbDatabase for MockDatabase<P> {
    type Transaction = MockTransaction<P>;

    fn create_transaction(&self) -> MockTransaction<P> {
        // Real FDB: a `get_read_version` RPC round-trip to a proxy and the
        // master. Here: one `u64` read under the cluster's mutex.
        let read_version = self.inner.lock().expect("MockFdb mutex poisoned").version;
        MockTransaction {
            db: self.inner.clone(),
            providers: self.providers.clone(),
            read_version,
            read_conflict_ranges: Vec::new(),
            write_set: BTreeMap::new(),
        }
    }
}

// ============================================================================
// MockTransaction — implements FdbTransaction
// ============================================================================

/// A transaction against [`MockDatabase`]. Reads are snapshot-isolated; writes
/// are buffered locally and applied atomically at commit.
pub struct MockTransaction<P: Providers> {
    db: Arc<Mutex<MockFdb>>,
    providers: P,
    read_version: Version,
    /// One half-open range per `get` call — the read conflict footprint
    /// checked at commit.
    read_conflict_ranges: Vec<(Key, Key)>,
    /// `None` is a tombstone.
    write_set: BTreeMap<Key, Option<Value>>,
}

impl<P: Providers> FdbTransaction for MockTransaction<P> {
    async fn get(&mut self, key: Key) -> Result<Option<Value>, FdbError> {
        // Sample latency *outside* the lock and sleep via the time provider.
        // Under SimProviders this advances simulation time; under
        // TokioProviders this is `tokio::time::sleep`.
        let latency = sample_latency(&self.providers, &self.db, |c| c.get_latency_ms.clone());
        self.providers
            .time()
            .sleep(latency)
            .await
            .expect("time provider sleep failed");

        // Read-your-write: a buffered local write wins over the snapshot.
        if let Some(buffered) = self.write_set.get(&key) {
            return Ok(buffered.clone());
        }

        // Every read adds an implicit single-key conflict range. `next_key`
        // produces the smallest key strictly greater than `key`, so the
        // half-open range covers exactly `key`.
        self.read_conflict_ranges
            .push((key.clone(), next_key(&key)));

        let too_old_prob = {
            let fdb = self.db.lock().expect("MockFdb mutex poisoned");
            fdb.config.transaction_too_old_prob
        };
        if self.providers.random().random_bool(too_old_prob) {
            return Err(FdbError::TransactionTooOld);
        }

        let fdb = self.db.lock().expect("MockFdb mutex poisoned");
        Ok(fdb.read_at(&key, self.read_version))
    }

    fn put(&mut self, key: Key, value: Value) {
        self.write_set.insert(key, Some(value));
    }

    fn delete(&mut self, key: Key) {
        self.write_set.insert(key, None);
    }

    async fn commit(self) -> Result<Version, FdbError> {
        let latency = sample_latency(&self.providers, &self.db, |c| c.commit_latency_ms.clone());
        self.providers
            .time()
            .sleep(latency)
            .await
            .expect("time provider sleep failed");

        let (not_committed_prob, unknown_result_prob) = {
            let fdb = self.db.lock().expect("MockFdb mutex poisoned");
            (
                fdb.config.not_committed_prob,
                fdb.config.commit_unknown_result_prob,
            )
        };

        let mut fdb = self.db.lock().expect("MockFdb mutex poisoned");

        // This loop *is* FDB's resolver. In a real cluster the resolver is
        // its own cluster role with its own process, receiving RPCs from
        // commit proxies. Here it's an inline `for` because there's no
        // network when "the cluster" is one mutex.
        for (begin, end) in &self.read_conflict_ranges {
            if fdb.any_write_in_range_since(begin, end, self.read_version) {
                return Err(FdbError::NotCommitted);
            }
        }

        // Synthetic conflict — fail *before* applying mutations, so a real
        // production layer cannot tell the difference from a real conflict.
        if self.providers.random().random_bool(not_committed_prob) {
            return Err(FdbError::NotCommitted);
        }

        // Assign version and apply. `next_commit_version` reads
        // `providers.time().now()` for the µs clock, so the version
        // sequence is deterministic under SimProviders.
        let commit_version = fdb.next_commit_version(self.providers.time());
        for (key, value) in self.write_set {
            fdb.data
                .entry(key)
                .or_default()
                .push((commit_version, value));
        }

        // Commit-unknown-result — by design fires *after* mutations land.
        // A naive retry here would double-apply; this is exactly the trap
        // that motivates FDB's idempotency guidance.
        if self.providers.random().random_bool(unknown_result_prob) {
            return Err(FdbError::CommitUnknownResult);
        }

        Ok(commit_version)
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Smallest key strictly greater than `key`. Used to turn a single-key read
/// into a half-open conflict range.
fn next_key(key: &Key) -> Key {
    let mut next = Vec::with_capacity(key.len() + 1);
    next.extend_from_slice(key);
    next.push(0);
    Bytes::from(next)
}

/// Sample a latency: pull the range out of the cluster config under the
/// mutex (released before we touch the RNG), then draw from the random
/// provider. Provider call is outside the lock so the random stream stays
/// independent of locking order.
fn sample_latency<P, F>(providers: &P, db: &Arc<Mutex<MockFdb>>, range_of: F) -> Duration
where
    P: Providers,
    F: FnOnce(&MockConfig) -> Range<u64>,
{
    let range = {
        let fdb = db.lock().expect("MockFdb mutex poisoned");
        range_of(&fdb.config)
    };
    let ms = if range.is_empty() {
        0
    } else {
        providers.random().random_range(range)
    };
    Duration::from_millis(ms)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use moonpool_sim::TokioProviders;

    fn k(s: &str) -> Key {
        Bytes::copy_from_slice(s.as_bytes())
    }

    fn deterministic() -> MockDatabase<TokioProviders> {
        MockDatabase::new(TokioProviders::new(), MockConfig::deterministic())
    }

    #[tokio::test]
    async fn put_then_get_across_transactions() {
        let db = deterministic();

        let mut tx = db.create_transaction();
        tx.put(k("hello"), k("world"));
        tx.commit().await.unwrap();

        let mut tx = db.create_transaction();
        assert_eq!(tx.get(k("hello")).await.unwrap(), Some(k("world")));
    }

    #[tokio::test]
    async fn read_your_write_within_transaction() {
        let db = deterministic();
        let mut tx = db.create_transaction();
        tx.put(k("k"), k("v"));
        assert_eq!(tx.get(k("k")).await.unwrap(), Some(k("v")));
    }

    #[tokio::test]
    async fn snapshot_isolation_old_reader_sees_old_value() {
        let db = deterministic();

        let mut tx = db.create_transaction();
        tx.put(k("k"), k("v1"));
        tx.commit().await.unwrap();

        // Reader A captures its read_version here, before B's commit.
        let mut tx_a = db.create_transaction();

        let mut tx_b = db.create_transaction();
        tx_b.put(k("k"), k("v2"));
        tx_b.commit().await.unwrap();

        // A still sees v1 — its snapshot predates B's commit.
        assert_eq!(tx_a.get(k("k")).await.unwrap(), Some(k("v1")));
    }

    #[tokio::test]
    async fn read_write_conflict_at_commit() {
        let db = deterministic();

        let mut seed = db.create_transaction();
        seed.put(k("counter"), k("0"));
        seed.commit().await.unwrap();

        let mut tx_a = db.create_transaction();
        let mut tx_b = db.create_transaction();

        // Both read, both want to write. B commits first.
        let _ = tx_a.get(k("counter")).await.unwrap();
        let _ = tx_b.get(k("counter")).await.unwrap();
        tx_b.put(k("counter"), k("1"));
        tx_b.commit().await.unwrap();

        // A's commit must fail — its read conflict range covers `counter`,
        // and B wrote there after A's read_version.
        tx_a.put(k("counter"), k("99"));
        assert_eq!(tx_a.commit().await, Err(FdbError::NotCommitted));
    }

    #[tokio::test]
    async fn delete_then_get_returns_none() {
        let db = deterministic();

        let mut tx = db.create_transaction();
        tx.put(k("k"), k("v"));
        tx.commit().await.unwrap();

        let mut tx = db.create_transaction();
        tx.delete(k("k"));
        tx.commit().await.unwrap();

        let mut tx = db.create_transaction();
        assert_eq!(tx.get(k("k")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn chaos_eventually_produces_all_three_errors() {
        // With chaos enabled, a bounded loop witnesses every error variant.
        // Under TokioProviders the RNG is thread-local; under SimProviders
        // this same code would be byte-identical across seeded reruns.
        let db = MockDatabase::new(TokioProviders::new(), MockConfig::chaos());

        let (mut saw_not_committed, mut saw_unknown, mut saw_too_old) = (false, false, false);

        for i in 0..1000_u64 {
            let mut tx = db.create_transaction();
            // Touch a unique key per iteration so semantic conflicts stay rare
            // and synthetic chaos dominates the observed error mix.
            let key = Bytes::from(format!("k/{i}"));
            match tx.get(key.clone()).await {
                Err(FdbError::TransactionTooOld) => {
                    saw_too_old = true;
                    continue;
                }
                Err(other) => panic!("unexpected get error: {other:?}"),
                Ok(_) => {}
            }
            tx.put(key, Bytes::from_static(b"v"));
            match tx.commit().await {
                Ok(_) => {}
                Err(FdbError::NotCommitted) => saw_not_committed = true,
                Err(FdbError::CommitUnknownResult) => saw_unknown = true,
                Err(FdbError::TransactionTooOld) => saw_too_old = true,
            }
            if saw_not_committed && saw_unknown && saw_too_old {
                return;
            }
        }
        panic!(
            "did not observe all errors in 1000 iterations: \
             not_committed={saw_not_committed} unknown={saw_unknown} too_old={saw_too_old}"
        );
    }
}
