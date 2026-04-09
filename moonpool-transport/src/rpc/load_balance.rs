//! Load-balancing primitives over groups of equivalent [`ServiceEndpoint`]s.
//!
//! This module provides a port of FoundationDB's `loadBalance()` from
//! `fdbrpc/include/fdbrpc/LoadBalance.actor.h`. The pieces are:
//!
//! - [`AtMostOnce`] — caller flag controlling retry on `MaybeDelivered` errors
//! - [`Distance`] — locality tag attached to each alternative
//! - [`Alternatives`] — sorted list of equivalent endpoints with a "best
//!   distance" count
//! - `QueueModel` / `ModelHolder` — smoothed per-endpoint latency and outstanding
//!   tracking (added in a follow-up commit)
//! - `load_balance()` — alternative selection + retry loop (added in a
//!   follow-up commit)
//!
//! See `docs/analysis/foundationdb/layer-3-fdbrpc.md` and the source files
//! referenced inline.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use moonpool_sim::assert_sometimes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::instrument;

use crate::error::MessagingError;
use crate::rpc::failure_monitor::FailureStatus;
use crate::rpc::{FailureMonitor, RpcError, ServiceEndpoint, Smoother};
use crate::{MessageCodec, NetTransport, Providers, TimeProvider};

/// E-folding time constant for the smoothed outstanding count, matching FDB's
/// `FLOW_KNOBS->QUEUE_MODEL_SMOOTHING_AMOUNT` (1 second by default).
const SMOOTHING_E_FOLDING: Duration = Duration::from_secs(1);

/// Initial latency for a freshly observed endpoint. Matches FDB's `1ms` seed
/// value in `QueueModel.h`.
const INITIAL_LATENCY: Duration = Duration::from_millis(1);

/// Whether the caller's request has observable side effects.
///
/// Mirrors FDB's `AtMostOnce` flag from
/// `fdbrpc/include/fdbrpc/LoadBalance.actor.h:572-625`.
///
/// - [`AtMostOnce::True`] — the request is non-idempotent (e.g., a commit). If
///   the load balancer hits `MaybeDelivered` / `BrokenPromise` it must **not**
///   retry on another alternative; the caller has to handle the ambiguity.
/// - [`AtMostOnce::False`] — the request is idempotent (e.g., a read). The
///   load balancer is free to retry on the next alternative on
///   `MaybeDelivered`.
///
/// Modelled as an enum (not a `bool`) per Rust API guideline C-CUSTOM-TYPE,
/// so call sites read clearly: `load_balance(..., AtMostOnce::True, ...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AtMostOnce {
    /// Side-effecting request — never retry on `MaybeDelivered`.
    True,
    /// Idempotent request — safe to retry on the next alternative.
    False,
}

/// Locality distance from the caller to an alternative.
///
/// Mirrors FDB's `LBDistance` (`fdbrpc/include/fdbrpc/MultiInterface.h:64-69`).
/// Lower is closer. Closer alternatives are preferred — see
/// [`Alternatives::count_best`] for how the load balancer biases toward the
/// nearest tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Distance {
    /// Same physical machine (loopback / unix socket).
    SameMachine,
    /// Same datacenter / availability zone.
    SameDc,
    /// Cross-datacenter / cross-region.
    Remote,
}

/// A group of equivalent backends for a single logical service.
///
/// Holds owned [`ServiceEndpoint`]s tagged with a [`Distance`], sorted in
/// ascending distance order so that the lowest indices are the closest
/// alternatives. [`Alternatives::count_best`] reports how many entries share
/// the closest distance — the load balancer prefers picking from this prefix
/// before falling back to remote alternatives.
///
/// Mirrors FDB's `MultiInterface<T>`
/// (`fdbrpc/include/fdbrpc/MultiInterface.h:196-212`).
///
/// # Bounds
///
/// `C: MessageCodec` is required because the struct stores
/// [`ServiceEndpoint<Req, Resp, C>`], whose own struct bound the compiler
/// propagates here. This is the one place this guideline-violating bound is
/// unavoidable; downstream impl blocks omit any extra bounds.
#[derive(Debug)]
pub struct Alternatives<Req, Resp, C: MessageCodec> {
    /// Sorted by `Distance` ascending. Entries within the same distance keep
    /// their input order — the caller is free to randomize before
    /// constructing if desired.
    entries: Vec<(ServiceEndpoint<Req, Resp, C>, Distance)>,
    /// Number of entries that share the minimum distance (the "local DC"
    /// prefix). `0` iff `entries` is empty.
    count_best: usize,
}

// Manual `Clone` (not derived) so the bound `C: Clone` lives on the impl
// rather than on the struct definition (C-STRUCT-BOUNDS).
impl<Req, Resp, C: MessageCodec + Clone> Clone for Alternatives<Req, Resp, C> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            count_best: self.count_best,
        }
    }
}

impl<Req, Resp, C: MessageCodec> Alternatives<Req, Resp, C> {
    /// Build from any iterable of `(endpoint, distance)` pairs.
    ///
    /// Sorts internally by ascending distance and computes `count_best`.
    /// An empty input is allowed; methods on the resulting `Alternatives`
    /// will report `len() == 0`.
    pub fn new(
        entries: impl IntoIterator<Item = (ServiceEndpoint<Req, Resp, C>, Distance)>,
    ) -> Self {
        let mut entries: Vec<_> = entries.into_iter().collect();
        // Stable sort: keep relative order of same-distance entries so callers
        // can pre-shuffle and have that shuffle preserved within each tier.
        entries.sort_by_key(|(_, d)| *d);
        let count_best = match entries.first() {
            None => 0,
            Some((_, best)) => entries.iter().take_while(|(_, d)| d == best).count(),
        };
        Self {
            entries,
            count_best,
        }
    }

    /// Number of alternatives in the group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` iff the group has no alternatives.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of entries at the minimum distance (the "best" tier).
    ///
    /// FDB equivalent: `MultiInterface::countBest`.
    #[must_use]
    pub fn count_best(&self) -> usize {
        self.count_best
    }

    /// Borrow the endpoint at index `i` (sorted order — `0..count_best()`
    /// are the closest tier).
    ///
    /// # Panics
    ///
    /// Panics if `i >= self.len()`.
    #[must_use]
    pub fn endpoint(&self, i: usize) -> &ServiceEndpoint<Req, Resp, C> {
        &self.entries[i].0
    }

    /// Distance tag of the entry at index `i`.
    ///
    /// # Panics
    ///
    /// Panics if `i >= self.len()`.
    #[must_use]
    pub fn distance(&self, i: usize) -> Distance {
        self.entries[i].1
    }
}

/// Per-endpoint smoothed latency / outstanding tracking shared across many
/// concurrent `load_balance` calls.
///
/// The `QueueModel` is keyed by [`UID::first`](crate::UID) — the high half of
/// the endpoint token, matching FDB's
/// `QueueModel::getMeasurement(token.first())` convention.
///
/// Uses interior mutability so a single `QueueModel` (typically wrapped in
/// `Rc`) can be shared by every in-flight request on the single-threaded
/// runtime — mirrors how FDB shares one `QueueModel` across all callers of a
/// `MultiInterface`.
#[derive(Debug, Default)]
pub struct QueueModel {
    inner: RefCell<HashMap<u64, QueueData>>,
}

#[derive(Debug)]
struct QueueData {
    smooth_outstanding: Smoother,
    latency: Duration,
    /// Server-reported penalty multiplier (`>= 1.0`). v1 always leaves this at
    /// `1.0`; the field exists to match FDB and to make adding server-side
    /// penalty reporting a one-line change later.
    penalty: f64,
    /// Wall-clock time (as a [`Duration`] from `TimeProvider::now`) until
    /// which the load balancer should avoid this endpoint. v1 never sets it
    /// — it stays at [`Duration::ZERO`]. Reserved for `future_version`-style
    /// errors when those are added to `ReplyError`.
    failed_until: Duration,
}

impl QueueData {
    fn new(now: Duration) -> Self {
        Self {
            smooth_outstanding: Smoother::new(SMOOTHING_E_FOLDING, now),
            latency: INITIAL_LATENCY,
            penalty: 1.0,
            failed_until: Duration::ZERO,
        }
    }
}

impl QueueModel {
    /// Create an empty model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct endpoints currently tracked.
    #[must_use]
    pub fn tracked_endpoints(&self) -> usize {
        self.inner.borrow().len()
    }

    /// Record the start of a request to `key`.
    ///
    /// Lazily inserts a fresh [`QueueData`] for unknown endpoints. Bumps the
    /// smoothed outstanding count by `1.0` (v1 always uses unit weight; FDB
    /// scales by `penalty` which is unused here).
    pub fn add_request(&self, key: u64, now: Duration) {
        let mut inner = self.inner.borrow_mut();
        let qd = inner.entry(key).or_insert_with(|| QueueData::new(now));
        qd.smooth_outstanding.add_delta(1.0, now);
    }

    /// Record the end of a request to `key`.
    ///
    /// - Subtracts `1.0` from the smoothed outstanding count.
    /// - On `received_response == true` the latency is **overwritten** with
    ///   the new measurement.
    /// - On `received_response == false` the latency is set to
    ///   `max(old, latency)` so a single failure cannot drop the metric below
    ///   the previously observed worst case (matches FDB
    ///   `QueueData::endRequest`).
    /// - No-op if `key` was never `add_request`'d (defensive against
    ///   double-release).
    pub fn end_request(&self, key: u64, latency: Duration, received_response: bool, now: Duration) {
        let mut inner = self.inner.borrow_mut();
        let Some(qd) = inner.get_mut(&key) else {
            return;
        };
        qd.smooth_outstanding.add_delta(-1.0, now);
        if received_response {
            qd.latency = latency;
        } else {
            qd.latency = qd.latency.max(latency);
        }
    }

    /// Smoothed outstanding count for `key` at simulation time `now`.
    ///
    /// Returns `0.0` for endpoints that have never been recorded — a brand new
    /// alternative looks maximally attractive, matching FDB's behavior of
    /// inserting a fresh `QueueData` with zero `smoothOutstanding`.
    #[must_use]
    pub fn smoothed_outstanding(&self, key: u64, now: Duration) -> f64 {
        let mut inner = self.inner.borrow_mut();
        match inner.get_mut(&key) {
            Some(qd) => qd.smooth_outstanding.smooth_total(now),
            None => 0.0,
        }
    }

    /// Last observed latency for `key`, or [`INITIAL_LATENCY`] if unknown.
    #[must_use]
    pub fn latency(&self, key: u64) -> Duration {
        self.inner
            .borrow()
            .get(&key)
            .map_or(INITIAL_LATENCY, |qd| qd.latency)
    }

    /// `failed_until` watermark for `key`. Always [`Duration::ZERO`] in v1
    /// (reserved for future `future_version`-style backoff).
    #[must_use]
    pub fn failed_until(&self, key: u64) -> Duration {
        self.inner
            .borrow()
            .get(&key)
            .map_or(Duration::ZERO, |qd| qd.failed_until)
    }

    /// Server-reported penalty for `key` (always `1.0` in v1).
    #[must_use]
    pub fn penalty(&self, key: u64) -> f64 {
        self.inner.borrow().get(&key).map_or(1.0, |qd| qd.penalty)
    }
}

/// RAII guard tying a request to its [`QueueModel`] slot.
///
/// Construct one immediately before issuing a request: it bumps
/// `smooth_outstanding` for the endpoint. On `release` it computes the latency
/// from the stored start time and records the outcome. If dropped without
/// `release` (e.g., the future was cancelled or panicked), the destructor
/// records the request as failed-with-elapsed-latency so a hung future does
/// not leak outstanding count forever — matches FDB `ModelHolder`'s
/// destructor behavior in `LoadBalance.actor.h:51-73`.
///
/// `T: TimeProvider` is held by value (cloned at construction) so the `Drop`
/// impl can read the current simulation time without a borrow.
#[derive(Debug)]
pub struct ModelHolder<'a, T: TimeProvider> {
    model: &'a QueueModel,
    key: u64,
    start: Duration,
    time: T,
    released: bool,
}

impl<'a, T: TimeProvider> ModelHolder<'a, T> {
    /// Begin tracking a request to `key`. Calls
    /// [`QueueModel::add_request`] immediately.
    #[must_use]
    pub fn new(model: &'a QueueModel, key: u64, time: &T) -> Self {
        let start = time.now();
        model.add_request(key, start);
        Self {
            model,
            key,
            start,
            time: time.clone(),
            released: false,
        }
    }

    /// Endpoint UID first-half this holder is tracking.
    #[must_use]
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Record the request outcome and consume the holder.
    ///
    /// Computes `latency = now - start` (saturating at zero) and forwards to
    /// [`QueueModel::end_request`]. After this call the destructor is a
    /// no-op.
    pub fn release(mut self, received_response: bool) {
        let now = self.time.now();
        let latency = now.saturating_sub(self.start);
        self.model
            .end_request(self.key, latency, received_response, now);
        self.released = true;
    }
}

impl<'a, T: TimeProvider> Drop for ModelHolder<'a, T> {
    fn drop(&mut self) {
        if !self.released {
            let now = self.time.now();
            let latency = now.saturating_sub(self.start);
            self.model.end_request(self.key, latency, false, now);
        }
    }
}

/// Backoff applied after every alternative has been tried and failed in a
/// single cycle. Matches FDB's `LOAD_BALANCE_START_BACKOFF` constant in
/// spirit (FDB caps at `LOAD_BALANCE_MAX_BACKOFF`; v1 uses a fixed value).
const ALL_ALTERNATIVES_FAILED_DELAY: Duration = Duration::from_millis(50);

/// Maximum number of full cycles through `alternatives` before giving up.
///
/// Each cycle visits every alternative at most once. After `MAX_FULL_CYCLES`
/// cycles `load_balance` returns the most recent error rather than retrying
/// indefinitely. The caller is expected to retry at a higher level if needed.
const MAX_FULL_CYCLES: usize = 2;

/// Pick the best available alternative not yet tried in the current cycle.
///
/// Implements FDB's locality-then-queue-depth ranking from
/// `fdbrpc/include/fdbrpc/LoadBalance.actor.h:719-815`:
///
/// 1. **Local first** — only consider entries inside the `count_best` prefix
///    (the same-distance "local" tier). If any local entry is available it
///    wins, regardless of how a remote entry might score.
/// 2. **Lowest smoothed outstanding** — among the candidates in the chosen
///    tier, pick the one with the lowest [`QueueModel::smoothed_outstanding`].
/// 3. **Skip failed** — entries whose `FailureMonitor` state is `Failed` or
///    whose `failed_until` is in the future are excluded.
fn pick_best_alternative<Req, Resp, C: MessageCodec>(
    alternatives: &Alternatives<Req, Resp, C>,
    fm: &FailureMonitor,
    model: &QueueModel,
    tried: &HashSet<usize>,
    now: Duration,
) -> Option<usize> {
    let score = |i: usize| -> f64 {
        let key = alternatives.endpoint(i).endpoint().token.first;
        model.smoothed_outstanding(key, now)
    };
    let viable = |i: usize| -> bool {
        if tried.contains(&i) {
            return false;
        }
        let endpoint = alternatives.endpoint(i).endpoint();
        if fm.state(endpoint) == FailureStatus::Failed {
            return false;
        }
        if model.failed_until(endpoint.token.first) > now {
            return false;
        }
        true
    };

    // Try the local-DC prefix first.
    if let Some(best) = (0..alternatives.count_best())
        .filter(|&i| viable(i))
        .min_by(|&a, &b| {
            score(a)
                .partial_cmp(&score(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    {
        return Some(best);
    }

    // Fall back to remote alternatives.
    (alternatives.count_best()..alternatives.len())
        .filter(|&i| viable(i))
        .min_by(|&a, &b| {
            score(a)
                .partial_cmp(&score(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Pick one endpoint from `alternatives`, send `request`, and retry on the
/// next alternative on failure.
///
/// This is the moonpool analog of FDB's
/// `loadBalance()` (`fdbrpc/include/fdbrpc/LoadBalance.actor.h:823-1018`)
/// without hedging in v1. Hedging is reserved for a follow-up commit; the
/// `nextAlt` data structures are already in place via [`Alternatives`] and
/// [`QueueModel`].
///
/// # Algorithm
///
/// 1. [`pick_best_alternative`] picks the lowest-`smooth_outstanding` viable
///    endpoint, biased toward the local-DC prefix.
/// 2. A [`ModelHolder`] is created so the request is reflected in
///    `smooth_outstanding` for the duration of the call.
/// 3. The request is sent via `try_get_reply` (`AtMostOnce::True`) or
///    `get_reply` (`AtMostOnce::False`), matching FDB's
///    `LoadBalance.actor.h:572-625` `checkAndProcessResultImpl`.
/// 4. On success, [`ModelHolder::release(true)`](ModelHolder::release) is
///    called and the response is returned.
/// 5. On failure:
///    - The holder records the failure (latency = elapsed since start).
///    - If `at_most_once == AtMostOnce::True` and the error
///      [`is_maybe_delivered`](RpcError::is_maybe_delivered), the error is
///      returned immediately. The caller must handle the ambiguity (e.g.,
///      via Pattern 4: read-before-retry).
///    - Otherwise, the alternative is marked as tried-this-cycle and the
///      next iteration picks a different one.
/// 6. After cycling through every alternative once, the function sleeps for
///    [`ALL_ALTERNATIVES_FAILED_DELAY`] and starts a fresh cycle, up to
///    [`MAX_FULL_CYCLES`] cycles total. After that the most recent error is
///    returned to the caller.
///
/// # Errors
///
/// - [`RpcError::Reply(ReplyError::MaybeDelivered)`](crate::rpc::ReplyError::MaybeDelivered)
///   immediately when `at_most_once == AtMostOnce::True` and the chosen peer
///   disconnects.
/// - [`RpcError::Messaging(MessagingError::InvalidState)`](MessagingError::InvalidState)
///   when `alternatives.is_empty()` — there is nothing to balance against.
/// - The most recent underlying [`RpcError`] when every alternative has been
///   tried [`MAX_FULL_CYCLES`] times without success.
#[instrument(skip_all, fields(n = alternatives.len()))]
pub async fn load_balance<Req, Resp, P, C>(
    transport: &NetTransport<P>,
    alternatives: &Alternatives<Req, Resp, C>,
    request: Req,
    at_most_once: AtMostOnce,
    model: &QueueModel,
) -> Result<Resp, RpcError>
where
    Req: Serialize + Clone,
    Resp: DeserializeOwned + 'static,
    P: Providers,
    C: MessageCodec + Clone,
{
    if alternatives.is_empty() {
        return Err(RpcError::Messaging(MessagingError::InvalidState {
            message: "load_balance called with no alternatives".to_string(),
        }));
    }

    let fm = transport.failure_monitor();
    let time = transport.providers().time().clone();

    let mut tried: HashSet<usize> = HashSet::new();
    let mut last_error: Option<RpcError> = None;
    let mut cycle: usize = 0;

    loop {
        let now = time.now();
        let candidate = pick_best_alternative(alternatives, &fm, model, &tried, now);

        let Some(idx) = candidate else {
            // No viable alternative this cycle.
            if cycle + 1 >= MAX_FULL_CYCLES {
                assert_sometimes!(true, "load_balance_giving_up_after_cycles");
                return Err(last_error.unwrap_or_else(|| {
                    RpcError::Messaging(MessagingError::InvalidState {
                        message: "load_balance: no alternative available".to_string(),
                    })
                }));
            }
            assert_sometimes!(true, "load_balance_all_alternatives_failed_backoff");
            cycle += 1;
            tried.clear();
            // Best-effort backoff; ignore the time provider returning a
            // shutdown error.
            let _ = time.sleep(ALL_ALTERNATIVES_FAILED_DELAY).await;
            continue;
        };

        tried.insert(idx);
        let endpoint_handle = alternatives.endpoint(idx);
        let key = endpoint_handle.endpoint().token.first;

        let holder = ModelHolder::new(model, key, &time);

        let result = match at_most_once {
            AtMostOnce::True => {
                endpoint_handle
                    .try_get_reply(transport, request.clone())
                    .await
            }
            AtMostOnce::False => endpoint_handle.get_reply(transport, request.clone()).await,
        };

        match result {
            Ok(resp) => {
                holder.release(true);
                return Ok(resp);
            }
            Err(err) => {
                holder.release(false);
                if at_most_once == AtMostOnce::True && err.is_maybe_delivered() {
                    assert_sometimes!(true, "load_balance_at_most_once_short_circuit");
                    return Err(err);
                }
                assert_sometimes!(true, "load_balance_retry_next_alternative");
                last_error = Some(err);
                // Loop continues; tried.insert(idx) ensures we won't pick this
                // one again until the next cycle.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::rpc::ServiceEndpoint;
    use crate::{Endpoint, JsonCodec, NetworkAddress, UID};

    fn ep(token: u64) -> ServiceEndpoint<u32, u32, JsonCodec> {
        let addr = NetworkAddress::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 4500 + token as u16);
        ServiceEndpoint::new(Endpoint::new(addr, UID::new(token, 1)), JsonCodec)
    }

    #[test]
    fn at_most_once_is_an_enum() {
        // Type-check: doesn't matter what the values mean, just that it's an
        // enum and not a bool. The match exhaustiveness check is the proof.
        fn name(a: AtMostOnce) -> &'static str {
            match a {
                AtMostOnce::True => "true",
                AtMostOnce::False => "false",
            }
        }
        assert_eq!(name(AtMostOnce::True), "true");
        assert_eq!(name(AtMostOnce::False), "false");
    }

    #[test]
    fn distance_orders_local_first() {
        let mut v = vec![Distance::Remote, Distance::SameMachine, Distance::SameDc];
        v.sort();
        assert_eq!(
            v,
            vec![Distance::SameMachine, Distance::SameDc, Distance::Remote]
        );
    }

    #[test]
    fn empty_alternatives() {
        let alts: Alternatives<u32, u32, JsonCodec> = Alternatives::new(std::iter::empty());
        assert!(alts.is_empty());
        assert_eq!(alts.len(), 0);
        assert_eq!(alts.count_best(), 0);
    }

    #[test]
    fn alternatives_sort_by_distance() {
        let alts = Alternatives::new(vec![
            (ep(1), Distance::Remote),
            (ep(2), Distance::SameMachine),
            (ep(3), Distance::SameDc),
            (ep(4), Distance::SameMachine),
        ]);

        assert_eq!(alts.len(), 4);
        // Sorted: SameMachine (×2), SameDc (×1), Remote (×1)
        assert_eq!(alts.distance(0), Distance::SameMachine);
        assert_eq!(alts.distance(1), Distance::SameMachine);
        assert_eq!(alts.distance(2), Distance::SameDc);
        assert_eq!(alts.distance(3), Distance::Remote);

        // The two SameMachine entries are tokens 2 and 4 (preserving input order
        // within the tier thanks to stable sort).
        assert_eq!(alts.endpoint(0).endpoint().token.first, 2);
        assert_eq!(alts.endpoint(1).endpoint().token.first, 4);
    }

    #[test]
    fn count_best_matches_top_tier() {
        let alts = Alternatives::new(vec![
            (ep(1), Distance::SameDc),
            (ep(2), Distance::SameDc),
            (ep(3), Distance::SameDc),
            (ep(4), Distance::Remote),
        ]);
        assert_eq!(alts.count_best(), 3);
    }

    #[test]
    fn count_best_is_one_when_one_local_one_remote() {
        let alts = Alternatives::new(vec![
            (ep(1), Distance::SameMachine),
            (ep(2), Distance::Remote),
        ]);
        assert_eq!(alts.count_best(), 1);
    }

    #[test]
    fn count_best_equals_len_when_all_same() {
        let alts = Alternatives::new(vec![(ep(1), Distance::Remote), (ep(2), Distance::Remote)]);
        assert_eq!(alts.count_best(), 2);
        assert_eq!(alts.len(), 2);
    }

    // ---- QueueModel tests ----

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn queue_model_unknown_keys_have_default_values() {
        let qm = QueueModel::new();
        assert_eq!(qm.tracked_endpoints(), 0);
        assert_eq!(qm.smoothed_outstanding(99, ms(0)), 0.0);
        assert_eq!(qm.latency(99), INITIAL_LATENCY);
        assert_eq!(qm.failed_until(99), Duration::ZERO);
        assert_eq!(qm.penalty(99), 1.0);
    }

    #[test]
    fn queue_model_add_then_end_balances_outstanding() {
        let qm = QueueModel::new();
        qm.add_request(7, ms(0));
        // Smoother lags — at t=0 estimate is still 0 (no time has passed).
        assert_eq!(qm.smoothed_outstanding(7, ms(0)), 0.0);
        // After a long time, the estimate converges toward total = 1.0.
        assert!((qm.smoothed_outstanding(7, ms(10_000)) - 1.0).abs() < 1e-3);

        qm.end_request(7, ms(50), true, ms(10_000));
        // Total = 0 again; estimate decays back toward 0.
        assert!(qm.smoothed_outstanding(7, ms(20_000)) < 1e-3);
        assert_eq!(qm.latency(7), ms(50));
        assert_eq!(qm.tracked_endpoints(), 1);
    }

    #[test]
    fn queue_model_failed_request_keeps_max_latency() {
        let qm = QueueModel::new();
        qm.add_request(7, ms(0));
        qm.end_request(7, ms(20), true, ms(10));
        assert_eq!(qm.latency(7), ms(20));

        // A subsequent failure with a smaller "elapsed" must NOT lower the
        // recorded latency — failures should never look better than past
        // successes.
        qm.add_request(7, ms(11));
        qm.end_request(7, ms(5), false, ms(20));
        assert_eq!(qm.latency(7), ms(20));

        // Now a real failure with a larger latency raises the watermark.
        qm.add_request(7, ms(21));
        qm.end_request(7, ms(100), false, ms(121));
        assert_eq!(qm.latency(7), ms(100));
    }

    #[test]
    fn queue_model_end_request_on_unknown_key_is_noop() {
        let qm = QueueModel::new();
        // Must not panic, must not insert anything.
        qm.end_request(42, ms(10), true, ms(0));
        assert_eq!(qm.tracked_endpoints(), 0);
    }

    #[test]
    fn queue_model_two_endpoints_are_independent() {
        let qm = QueueModel::new();
        qm.add_request(1, ms(0));
        qm.add_request(2, ms(0));
        qm.end_request(1, ms(5), true, ms(5));
        // Endpoint 2 still outstanding.
        assert!(qm.smoothed_outstanding(2, ms(10_000)) > 0.5);
        assert!(qm.smoothed_outstanding(1, ms(10_000)) < 0.5);
    }

    // ---- ModelHolder tests ----

    /// Minimal `TimeProvider` implementation backed by a [`Cell<Duration>`]
    /// so tests can advance the clock manually.
    #[derive(Clone)]
    struct FakeTime(std::rc::Rc<std::cell::Cell<Duration>>);

    impl FakeTime {
        fn new(start: Duration) -> Self {
            Self(std::rc::Rc::new(std::cell::Cell::new(start)))
        }
        fn advance(&self, by: Duration) {
            self.0.set(self.0.get() + by);
        }
    }

    #[async_trait::async_trait(?Send)]
    impl TimeProvider for FakeTime {
        async fn sleep(&self, _duration: Duration) -> Result<(), crate::TimeError> {
            Ok(())
        }
        fn now(&self) -> Duration {
            self.0.get()
        }
        fn timer(&self) -> Duration {
            self.0.get()
        }
        async fn timeout<F, T>(&self, _duration: Duration, future: F) -> Result<T, crate::TimeError>
        where
            F: std::future::Future<Output = T>,
        {
            Ok(future.await)
        }
    }

    #[test]
    fn model_holder_release_records_latency() {
        let qm = QueueModel::new();
        let time = FakeTime::new(ms(100));
        let holder = ModelHolder::new(&qm, 7, &time);
        assert_eq!(holder.key(), 7);
        time.advance(ms(25));
        holder.release(true);
        assert_eq!(qm.latency(7), ms(25));
    }

    #[test]
    fn model_holder_drop_without_release_records_failure() {
        let qm = QueueModel::new();
        let time = FakeTime::new(ms(0));
        {
            let _holder = ModelHolder::new(&qm, 9, &time);
            time.advance(ms(7));
            // Drop here, no explicit release.
        }
        // Latency should at least be the elapsed time, recorded as a failure.
        assert!(qm.latency(9) >= ms(7));
        // Outstanding is back to zero (long-time-from-now).
        assert!(qm.smoothed_outstanding(9, ms(60_000)) < 1e-3);
    }

    #[test]
    fn model_holder_release_then_drop_does_not_double_count() {
        let qm = QueueModel::new();
        let time = FakeTime::new(ms(0));
        let holder = ModelHolder::new(&qm, 1, &time);
        time.advance(ms(10));
        holder.release(true);
        // After release, drop should be a no-op. Outstanding should be 0
        // (modulo smoother lag) at a far-future time.
        let later = qm.smoothed_outstanding(1, ms(60_000));
        assert!(later.abs() < 1e-3, "expected ~0, got {later}");
    }

    // ---- load_balance integration tests ----

    mod lb_integration {
        use std::rc::Rc;

        use super::super::{AtMostOnce, Distance, QueueModel, load_balance};
        use super::Alternatives;
        use crate::rpc::ReplyError;
        use crate::rpc::test_support::{Echo, dispatch_reply, make_transport, register_servers};

        #[test]
        fn empty_alternatives_returns_messaging_error() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build_local(tokio::runtime::LocalOptions::default())
                .expect("build runtime");

            rt.block_on(async {
                let transport = make_transport();
                let alts: Alternatives<Echo, Echo, crate::JsonCodec> =
                    Alternatives::new(std::iter::empty());
                let model = QueueModel::new();
                let result =
                    load_balance(&transport, &alts, Echo(0), AtMostOnce::False, &model).await;
                assert!(result.is_err());
            });
        }

        #[test]
        fn picks_first_available_alternative_and_returns_response() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build_local(tokio::runtime::LocalOptions::default())
                .expect("build runtime");

            rt.block_on(async {
                let transport = Rc::new(make_transport());
                let (queues, endpoints) = register_servers(&transport, &[100, 101, 102]);

                let alts = Alternatives::new(endpoints.into_iter().map(|e| (e, Distance::SameDc)));
                let model = QueueModel::new();

                let t = Rc::clone(&transport);
                let alts_rc = Rc::new(alts);
                let alts_for_task = Rc::clone(&alts_rc);
                let model_rc = Rc::new(model);
                let model_for_task = Rc::clone(&model_rc);
                let handle = tokio::task::spawn_local(async move {
                    load_balance(
                        &t,
                        &alts_for_task,
                        Echo(7),
                        AtMostOnce::False,
                        &model_for_task,
                    )
                    .await
                });

                tokio::task::yield_now().await;

                // Exactly one queue should have received the request — the
                // one that the load balancer chose.
                let mut answered = 0;
                for q in &queues {
                    if let Some(envelope) = q.try_recv() {
                        assert_eq!(envelope.request, Echo(7));
                        dispatch_reply(&transport, &envelope, Ok(Echo(900)));
                        answered += 1;
                    }
                }
                assert_eq!(answered, 1, "load_balance must hit exactly one peer");

                let resp = handle.await.expect("join task").expect("load_balance");
                assert_eq!(resp, Echo(900));

                // The chosen endpoint should have non-zero observed latency
                // recorded against it (the QueueModel was updated).
                let any_tracked = model_rc.tracked_endpoints();
                assert_eq!(any_tracked, 1, "model should have one entry");
            });
        }

        #[test]
        fn retries_on_next_alternative_after_broken_promise() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build_local(tokio::runtime::LocalOptions::default())
                .expect("build runtime");

            rt.block_on(async {
                let transport = Rc::new(make_transport());
                let (queues, endpoints) = register_servers(&transport, &[200, 201, 202]);
                let alts = Alternatives::new(endpoints.into_iter().map(|e| (e, Distance::SameDc)));
                let model = QueueModel::new();

                let t = Rc::clone(&transport);
                let alts_rc = Rc::new(alts);
                let model_rc = Rc::new(model);
                let alts_for_task = Rc::clone(&alts_rc);
                let model_for_task = Rc::clone(&model_rc);
                let handle = tokio::task::spawn_local(async move {
                    load_balance(
                        &t,
                        &alts_for_task,
                        Echo(11),
                        AtMostOnce::False,
                        &model_for_task,
                    )
                    .await
                });

                tokio::task::yield_now().await;

                // Find which queue got the request, fail it, then drive a
                // success on the next call.
                let mut first_idx = None;
                for (i, q) in queues.iter().enumerate() {
                    if let Some(envelope) = q.try_recv() {
                        first_idx = Some(i);
                        dispatch_reply(&transport, &envelope, Err(ReplyError::BrokenPromise));
                        break;
                    }
                }
                assert!(first_idx.is_some(), "first attempt should have hit a peer");

                tokio::task::yield_now().await;

                // The retry should have hit a different peer.
                let mut second_idx = None;
                for (i, q) in queues.iter().enumerate() {
                    if Some(i) == first_idx {
                        continue;
                    }
                    if let Some(envelope) = q.try_recv() {
                        second_idx = Some(i);
                        dispatch_reply(&transport, &envelope, Ok(Echo(777)));
                        break;
                    }
                }
                assert!(
                    second_idx.is_some(),
                    "retry should have hit a different peer"
                );
                assert_ne!(first_idx, second_idx);

                let resp = handle.await.expect("join task").expect("load_balance");
                assert_eq!(resp, Echo(777));
            });
        }

        #[test]
        fn at_most_once_short_circuits_on_maybe_delivered() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build_local(tokio::runtime::LocalOptions::default())
                .expect("build runtime");

            rt.block_on(async {
                let transport = Rc::new(make_transport());
                let (_queues, endpoints) = register_servers(&transport, &[300, 301]);
                let alts = Alternatives::new(endpoints.into_iter().map(|e| (e, Distance::SameDc)));
                let model = QueueModel::new();

                // We use try_get_reply (AtMostOnce::True). When the chosen
                // peer's address is marked Failed mid-call, the delivery
                // function returns MaybeDelivered, and load_balance must
                // propagate it without trying the next alternative.
                let t = Rc::clone(&transport);
                let alts_rc = Rc::new(alts);
                let model_rc = Rc::new(model);
                let alts_for_task = Rc::clone(&alts_rc);
                let model_for_task = Rc::clone(&model_rc);
                let handle = tokio::task::spawn_local(async move {
                    load_balance(
                        &t,
                        &alts_for_task,
                        Echo(13),
                        AtMostOnce::True,
                        &model_for_task,
                    )
                    .await
                });

                tokio::task::yield_now().await;

                // Mark the address Failed → the in-flight try_get_reply
                // should observe the disconnect signal and return
                // MaybeDelivered.
                transport
                    .failure_monitor()
                    .set_status("10.0.0.1:4500", super::FailureStatus::Failed);

                let result = handle.await.expect("join task");
                let err = result.expect_err("expected MaybeDelivered");
                assert!(err.is_maybe_delivered(), "got: {err:?}");
            });
        }
    }
}
