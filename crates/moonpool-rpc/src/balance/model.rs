//! The queue model: weighted outstanding work, measured latency, server
//! penalties and temporary exclusion per endpoint incarnation, plus the
//! shared hedge budget and the bounded collection of late losers
//! (`FoundationDB`'s `QueueModel`, `QueueData` and `ModelHolder`).
//!
//! Every started attempt takes one [`Reservation`] and gives it back
//! exactly once: [`Reservation::release`] consumes it, and dropping it
//! unreleased gives it back as an unclean outcome without a latency sample
//! (the `ModelHolder` destructor). There is no other way to end one, so a
//! reservation cannot leak or be released twice.
//!
//! The model never reads a clock or an RNG itself: callers pass provider
//! time and random draws in, which keeps it deterministic and testable.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::policy::{Backoff, validate_backoff};
use crate::config::InvalidConfig;
use crate::endpoint::Endpoint;

/// Penalties above this count as a server asking to be avoided
/// (`LOAD_BALANCE_PENALTY_IS_BAD`).
pub(crate) const BAD_PENALTY: f64 = 1.001;
/// The largest penalty a server can impose on itself.
const MAX_PENALTY: f64 = 1000.0;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("Mutex poisoned: prior task panicked")
}

/// The shared hedge budget (`secondBudget`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HedgeBudget {
    /// Budget when the model is created (`FoundationDB` starts at 0).
    pub initial: f64,
    /// Added whenever a primary attempt lands (reply or error) before any
    /// concurrent copy of its call was sent, as in `loadBalance`'s
    /// single-request wait (`SECOND_REQUEST_BUDGET_GROWTH`, 0.05).
    pub growth: f64,
    /// Upper bound (`SECOND_REQUEST_MAX_BUDGET`, 100).
    pub max: f64,
}

impl Default for HedgeBudget {
    fn default() -> Self {
        Self {
            initial: 0.0,
            growth: 0.05,
            max: 100.0,
        }
    }
}

/// Configuration of a [`QueueModel`]. Plain data.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelConfig {
    /// Latency assumed for an endpoint never measured (1 ms).
    pub initial_latency: Duration,
    /// The hedge budget every concurrent copy draws from.
    pub hedge_budget: HedgeBudget,
    /// Growth of the second-request multiplier per copy sent
    /// (`SECOND_REQUEST_MULTIPLIER_GROWTH`, 0.01).
    pub multiplier_growth: f64,
    /// Its decay at each budget growth (`SECOND_REQUEST_MULTIPLIER_DECAY`,
    /// 0.00025); it never falls below 1.
    pub multiplier_decay: f64,
    /// Temporary exclusion of an endpoint that is behind or refused for
    /// overload (`FUTURE_VERSION_{INITIAL,MAX}_BACKOFF`,
    /// `FUTURE_VERSION_BACKOFF_GROWTH`: 1 s doubling to 8 s), jittered.
    pub exclusion: Backoff,
    /// Late losers kept at once; the oldest is dropped (and its
    /// reservation released) past this bound.
    pub max_lagging: usize,
    /// How long a late loser is waited for, on provider time from the
    /// moment its call handed it over, before it is dropped. Enforced by
    /// [`QueueModel::collect_lagging`]; see there for an unpolled
    /// collector.
    pub lagging_timeout: Duration,
    /// Endpoints tracked at once; idle ones are forgotten, least recently
    /// used first. Forgetting scans the table (linear in this bound), and
    /// happens only when the table is full and a new endpoint appears.
    pub max_tracked_endpoints: usize,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            initial_latency: Duration::from_millis(1),
            hedge_budget: HedgeBudget::default(),
            multiplier_growth: 0.01,
            multiplier_decay: 0.000_25,
            exclusion: Backoff {
                initial: Duration::from_secs(1),
                max: Duration::from_secs(8),
                growth: 2.0,
                jitter: 0.25,
            },
            max_lagging: 256,
            lagging_timeout: Duration::from_secs(10),
            max_tracked_endpoints: 4096,
        }
    }
}

impl ModelConfig {
    /// Check the configuration.
    ///
    /// # Errors
    ///
    /// A description of the first invalid field.
    pub fn validate(&self) -> Result<(), InvalidConfig> {
        let budget = &self.hedge_budget;
        let finite = |value: f64| value.is_finite() && value >= 0.0;
        if !(finite(budget.initial) && finite(budget.growth) && finite(budget.max))
            || budget.initial > budget.max
        {
            return Err(InvalidConfig(
                "hedge_budget: finite, non-negative, initial <= max".into(),
            ));
        }
        if !(finite(self.multiplier_growth) && finite(self.multiplier_decay)) {
            return Err(InvalidConfig(
                "multiplier growth/decay: finite and non-negative".into(),
            ));
        }
        validate_backoff("exclusion", &self.exclusion)?;
        if self.max_lagging == 0 || self.max_tracked_endpoints == 0 {
            return Err(InvalidConfig(
                "max_lagging and max_tracked_endpoints must be positive".into(),
            ));
        }
        Ok(())
    }
}

/// What a server said about its own load alongside a usable reply
/// (`LoadBalancedReply::penalty`, `BasicLoadBalancedReply::processBusyTime`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Feedback {
    /// Cost of each request to this server (1 is normal; more asks
    /// clients to send less). Clamped to `1..=1000`.
    pub penalty: Option<f64>,
    /// A busy score for busy-score selection (higher is busier).
    pub busy: Option<u32>,
}

/// How one reservation ended, for the model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModelOutcome {
    /// A response: the latency sample replaces the previous one, the
    /// exclusion backoff resets and the feedback is recorded.
    Clean(Feedback),
    /// The endpoint is temporarily behind: exclude it for a growing,
    /// jittered backoff. Not a clean sample: the backoff does not reset.
    Behind,
    /// The endpoint refused for load (the application's classifier, or
    /// the transport's [`ErrorReason::Overloaded`](crate::ErrorReason::Overloaded)
    /// refusal with no feedback): excluded like [`Behind`](Self::Behind),
    /// and its feedback (a penalty) is recorded.
    Overloaded(Feedback),
    /// No usable response (disconnect, timeout, error): the latency can
    /// only grow.
    Failed,
}

/// One endpoint's current measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Weighted outstanding work: the sum of the penalties in force when
    /// each in-flight attempt started.
    pub outstanding: f64,
    /// Attempts in flight.
    pub in_flight: u32,
    /// Last clean latency sample (or a larger unclean one).
    pub latency: Duration,
    /// The server's latest penalty.
    pub penalty: f64,
    /// Excluded until this provider time, if still in the future.
    pub excluded_until: Option<Duration>,
    /// The latest busy score and when it arrived.
    pub busy: Option<(u32, Duration)>,
}

/// Counters of a [`QueueModel`], for tests and operations.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ModelStats {
    /// Reservations taken.
    pub reservations: u64,
    /// Reservations given back (explicitly or by drop).
    pub releases: u64,
    /// Reservations given back unreleased, by drop.
    pub dropped_releases: u64,
    /// Attempts in flight across every endpoint.
    pub in_flight: u64,
    /// Endpoints tracked.
    pub endpoints: usize,
    /// Exclusions applied.
    pub exclusions: u64,
    /// Late losers currently collected.
    pub lagging: usize,
    /// Late losers that completed and updated the model.
    pub lagging_completed: u64,
    /// Late losers dropped at their timeout.
    pub lagging_expired: u64,
    /// Late losers dropped because the collection was full.
    pub lagging_evicted: u64,
    /// The hedge budget now.
    pub hedge_budget: f64,
    /// The second-request multiplier now.
    pub multiplier: f64,
    /// Concurrent copies granted a budget unit.
    pub copies_granted: u64,
    /// Concurrent copies refused for lack of budget.
    pub copies_denied: u64,
}

#[derive(Debug, Clone)]
struct QueueData {
    outstanding: f64,
    in_flight: u32,
    latency: Duration,
    penalty: f64,
    excluded_until: Duration,
    backoff: Duration,
    increase_backoff_at: Duration,
    busy: Option<(u32, Duration)>,
    last_used: u64,
}

impl QueueData {
    fn new(latency: Duration) -> Self {
        Self {
            outstanding: 0.0,
            in_flight: 0,
            latency,
            penalty: 1.0,
            excluded_until: Duration::ZERO,
            backoff: Duration::ZERO,
            increase_backoff_at: Duration::ZERO,
            busy: None,
            last_used: 0,
        }
    }
}

struct State {
    data: BTreeMap<Endpoint, QueueData>,
    tick: u64,
    budget: f64,
    multiplier: f64,
    reservations: u64,
    releases: u64,
    dropped_releases: u64,
    exclusions: u64,
    copies_granted: u64,
    copies_denied: u64,
}

impl State {
    fn entry(&mut self, config: &ModelConfig, endpoint: Endpoint) -> &mut QueueData {
        self.tick += 1;
        if !self.data.contains_key(&endpoint) && self.data.len() >= config.max_tracked_endpoints {
            // Forget the least recently used idle endpoint; busy ones stay
            // (they are bounded by the attempts in flight).
            let idle = self
                .data
                .iter()
                .filter(|(_, data)| data.in_flight == 0)
                .min_by_key(|(_, data)| data.last_used)
                .map(|(endpoint, _)| *endpoint);
            if let Some(idle) = idle {
                self.data.remove(&idle);
            }
        }
        let tick = self.tick;
        let data = self
            .data
            .entry(endpoint)
            .or_insert_with(|| QueueData::new(config.initial_latency));
        data.last_used = tick;
        data
    }
}

struct LaggingSet {
    entries: VecDeque<Pin<Box<dyn Future<Output = LagEnd> + Send>>>,
    waker: Option<Waker>,
    /// Entries a collector is polling right now (outside the lock).
    out: usize,
    completed: u64,
    expired: u64,
    evicted: u64,
}

/// How a late loser ended.
pub(crate) enum LagEnd {
    /// Its outcome arrived and was released into the model.
    Completed,
    /// It was dropped at its timeout.
    Expired,
}

/// The shared queue model. Cheap to clone; clones share everything, so
/// several balanced clients (for example one per method of the same
/// servers) can share one model, as `FoundationDB` shares one per client.
#[derive(Clone)]
pub struct QueueModel {
    config: Arc<ModelConfig>,
    state: Arc<Mutex<State>>,
    lagging: Arc<Mutex<LaggingSet>>,
}

impl std::fmt::Debug for QueueModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueModel")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl QueueModel {
    /// A model with `config`.
    ///
    /// # Errors
    ///
    /// The first invalid field ([`ModelConfig::validate`]).
    pub fn new(config: ModelConfig) -> Result<Self, InvalidConfig> {
        config.validate()?;
        let state = State {
            data: BTreeMap::new(),
            tick: 0,
            budget: config.hedge_budget.initial,
            multiplier: 1.0,
            reservations: 0,
            releases: 0,
            dropped_releases: 0,
            exclusions: 0,
            copies_granted: 0,
            copies_denied: 0,
        };
        Ok(Self {
            config: Arc::new(config),
            state: Arc::new(Mutex::new(state)),
            lagging: Arc::new(Mutex::new(LaggingSet {
                entries: VecDeque::new(),
                waker: None,
                out: 0,
                completed: 0,
                expired: 0,
                evicted: 0,
            })),
        })
    }

    /// The model's configuration.
    #[must_use]
    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    /// The current measurement of `endpoint` at provider time `now`.
    #[must_use]
    pub fn measurement(&self, endpoint: &Endpoint, now: Duration) -> Measurement {
        let state = lock(&self.state);
        let data = state
            .data
            .get(endpoint)
            .cloned()
            .unwrap_or_else(|| QueueData::new(self.config.initial_latency));
        Measurement {
            outstanding: data.outstanding,
            in_flight: data.in_flight,
            latency: data.latency,
            penalty: data.penalty,
            excluded_until: (data.excluded_until > now).then_some(data.excluded_until),
            busy: data.busy,
        }
    }

    /// Start one attempt at `endpoint` (provider time `now`): add its
    /// current penalty to the outstanding work (`addRequest`).
    #[must_use = "a reservation is released when dropped"]
    pub fn reserve(&self, endpoint: Endpoint, now: Duration) -> Reservation {
        let mut state = lock(&self.state);
        state.reservations += 1;
        let data = state.entry(&self.config, endpoint);
        data.in_flight += 1;
        data.outstanding += data.penalty;
        let delta = data.penalty;
        Reservation {
            state: Arc::clone(&self.state),
            config: Arc::clone(&self.config),
            endpoint,
            delta,
            started: now,
            released: false,
        }
    }

    /// Take one budget unit for a concurrent copy; `false` when the
    /// budget is exhausted. A granted copy grows the second-request
    /// multiplier.
    #[must_use = "a granted unit is spent even if unused"]
    pub fn try_take_copy(&self) -> bool {
        let mut state = lock(&self.state);
        if state.budget >= 1.0 {
            state.budget -= 1.0;
            state.multiplier += self.config.multiplier_growth;
            state.copies_granted += 1;
            true
        } else {
            state.copies_denied += 1;
            false
        }
    }

    /// A primary attempt landed alone (no concurrent copy of its call was
    /// sent): grow the budget, decay the multiplier.
    pub fn first_response(&self) {
        let mut state = lock(&self.state);
        state.budget =
            (state.budget + self.config.hedge_budget.growth).min(self.config.hedge_budget.max);
        state.multiplier = (state.multiplier - self.config.multiplier_decay).max(1.0);
    }

    /// The second-request multiplier now.
    #[must_use]
    pub fn multiplier(&self) -> f64 {
        lock(&self.state).multiplier
    }

    /// Counters now.
    #[must_use]
    pub fn stats(&self) -> ModelStats {
        let model = lock(&self.state);
        let mut stats = ModelStats {
            reservations: model.reservations,
            releases: model.releases,
            dropped_releases: model.dropped_releases,
            in_flight: model
                .data
                .values()
                .map(|data| u64::from(data.in_flight))
                .sum(),
            endpoints: model.data.len(),
            exclusions: model.exclusions,
            hedge_budget: model.budget,
            multiplier: model.multiplier,
            copies_granted: model.copies_granted,
            copies_denied: model.copies_denied,
            ..ModelStats::default()
        };
        drop(model);
        let lagging = lock(&self.lagging);
        stats.lagging = lagging.entries.len() + lagging.out;
        stats.lagging_completed = lagging.completed;
        stats.lagging_expired = lagging.expired;
        stats.lagging_evicted = lagging.evicted;
        stats
    }

    /// Hand over a late loser; it owns its reservation and releases it
    /// when it ends. Past [`ModelConfig::max_lagging`] the oldest one is
    /// dropped (its reservation released unclean, its reply route freed).
    pub(crate) fn push_lagging(&self, entry: Pin<Box<dyn Future<Output = LagEnd> + Send>>) {
        let (evicted, waker) = {
            let mut lagging = lock(&self.lagging);
            let mut evicted = Vec::new();
            // Entries a collector holds right now count too; it enforces
            // the bound again when it puts them back.
            while lagging.entries.len() + lagging.out >= self.config.max_lagging {
                let Some(oldest) = lagging.entries.pop_front() else {
                    break;
                };
                lagging.evicted += 1;
                evicted.push(oldest);
            }
            lagging.entries.push_back(entry);
            (evicted, lagging.waker.take())
        };
        // Dropped outside the lock: each releases its reservation.
        drop(evicted);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Drive late losers (attempts still in flight when their call
    /// finished or was dropped) so their outcomes update the model and
    /// their reservations are released with a latency sample.
    ///
    /// Poll it next to the application for the model's whole life, like
    /// [`RpcDriver::run`](crate::RpcDriver::run); it never completes. Each
    /// late loser ends when its outcome arrives or when
    /// [`ModelConfig::lagging_timeout`] has passed since its hand-off,
    /// whichever comes first. If the collector is not polled, only the
    /// count bound holds: past [`ModelConfig::max_lagging`] the oldest are
    /// dropped, the others wait (their outcomes unobserved, their
    /// reservations held) until the collector runs or the model is
    /// dropped. Every one still releases its reservation exactly once.
    pub fn collect_lagging(&self) -> impl Future<Output = ()> + Send + 'static {
        let lagging = Arc::clone(&self.lagging);
        let max = self.config.max_lagging;
        futures::future::poll_fn(move |cx: &mut Context<'_>| {
            // Poll outside the lock: an ending entry releases its
            // reservation and reports to hooks, which may read the model.
            let mut entries = {
                let mut set = lock(&lagging);
                set.waker = Some(cx.waker().clone());
                let entries = std::mem::take(&mut set.entries);
                set.out += entries.len();
                entries
            };
            let taken = entries.len();
            let (mut completed, mut expired) = (0, 0);
            let mut pending = VecDeque::with_capacity(taken);
            for mut entry in entries.drain(..) {
                match entry.as_mut().poll(cx) {
                    Poll::Ready(LagEnd::Completed) => completed += 1,
                    Poll::Ready(LagEnd::Expired) => expired += 1,
                    Poll::Pending => pending.push_back(entry),
                }
            }
            let evicted = {
                let mut set = lock(&lagging);
                set.out -= taken;
                set.completed += completed;
                set.expired += expired;
                // Entries pushed meanwhile are newer: keep them behind.
                pending.append(&mut set.entries);
                set.entries = pending;
                let mut evicted = Vec::new();
                while set.entries.len() > max {
                    if let Some(oldest) = set.entries.pop_front() {
                        set.evicted += 1;
                        evicted.push(oldest);
                    }
                }
                evicted
            };
            drop(evicted);
            Poll::Pending
        })
    }
}

/// One attempt's claim on the model: reserved when the attempt starts,
/// released exactly once when it ends (`ModelHolder`).
///
/// [`release`](Self::release) consumes it; dropping it unreleased gives
/// it back as an unclean outcome without a latency sample.
#[must_use = "a reservation is released when dropped"]
pub struct Reservation {
    state: Arc<Mutex<State>>,
    config: Arc<ModelConfig>,
    endpoint: Endpoint,
    delta: f64,
    started: Duration,
    released: bool,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("endpoint", &self.endpoint)
            .field("delta", &self.delta)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl Reservation {
    /// The endpoint incarnation it is held on.
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// Provider time when it was taken.
    #[must_use]
    pub fn started(&self) -> Duration {
        self.started
    }

    /// Give it back at provider time `now` with `outcome` (`endRequest`);
    /// `draw` (uniform in `0.0..1.0`, from the provider RNG) jitters an
    /// exclusion.
    pub fn release(mut self, now: Duration, outcome: ModelOutcome, draw: f64) {
        self.finish(Some((now, outcome, draw)));
    }

    fn finish(&mut self, how: Option<(Duration, ModelOutcome, f64)>) {
        if self.released {
            return;
        }
        self.released = true;
        let mut state = lock(&self.state);
        state.releases += 1;
        if how.is_none() {
            state.dropped_releases += 1;
        }
        let config = Arc::clone(&self.config);
        let data = state.entry(&config, self.endpoint);
        data.in_flight = data.in_flight.saturating_sub(1);
        data.outstanding -= self.delta;
        if data.in_flight == 0 {
            // No float drift survives an idle endpoint.
            data.outstanding = 0.0;
        }
        let Some((now, outcome, draw)) = how else {
            return;
        };
        let sample = now.saturating_sub(self.started);
        let mut excluded = false;
        match outcome {
            ModelOutcome::Clean(feedback) => {
                data.latency = sample;
                data.backoff = Duration::ZERO;
                data.increase_backoff_at = Duration::ZERO;
                record_feedback(data, feedback, now);
            }
            ModelOutcome::Behind | ModelOutcome::Overloaded(_) => {
                // Not a clean sample: the latency can only grow, and the
                // exclusion backoff keeps growing until a clean reply.
                data.latency = data.latency.max(sample);
                if now >= data.increase_backoff_at {
                    data.backoff = config.exclusion.next(data.backoff);
                    data.increase_backoff_at = now + data.backoff;
                }
                data.excluded_until = now + config.exclusion.jittered(data.backoff, draw);
                excluded = true;
                if let ModelOutcome::Overloaded(feedback) = outcome {
                    record_feedback(data, feedback, now);
                }
            }
            ModelOutcome::Failed => {
                data.latency = data.latency.max(sample);
            }
        }
        if excluded {
            state.exclusions += 1;
        }
    }
}

fn record_feedback(data: &mut QueueData, feedback: Feedback, now: Duration) {
    if let Some(penalty) = feedback.penalty
        && penalty.is_finite()
        && penalty > 0.0
    {
        data.penalty = penalty.clamp(1.0, MAX_PENALTY);
    }
    if let Some(busy) = feedback.busy {
        data.busy = Some((busy, now));
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.finish(None);
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::{Feedback, LagEnd, ModelConfig, ModelOutcome, QueueModel};
    use crate::endpoint::{Endpoint, EndpointToken, Incarnation};

    fn endpoint(port: u16, incarnation: u128) -> Endpoint {
        Endpoint::new(
            SocketAddr::from(([10, 0, 1, 1], port)),
            Incarnation::from_raw(incarnation),
            EndpointToken::from_parts(1, 1),
        )
    }

    fn model() -> QueueModel {
        QueueModel::new(ModelConfig::default()).expect("valid default")
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn every_exit_path_releases_exactly_once() {
        let model = model();
        let a = endpoint(1, 1);
        // Success, failure, behind, and three drops (cancel, a late loser
        // dropped, a reservation never released).
        let paths = 6;
        let mut reservations: Vec<_> = (0..paths).map(|_| model.reserve(a, ms(0))).collect();
        assert_eq!(model.measurement(&a, ms(0)).in_flight, 6);
        assert!((model.measurement(&a, ms(0)).outstanding - 6.0).abs() < 1e-9);
        let behind = reservations.remove(0);
        behind.release(ms(5), ModelOutcome::Behind, 0.0);
        let failed = reservations.remove(0);
        failed.release(ms(7), ModelOutcome::Failed, 0.0);
        let clean = reservations.remove(0);
        clean.release(ms(9), ModelOutcome::Clean(Feedback::default()), 0.0);
        drop(reservations);
        let stats = model.stats();
        assert_eq!(stats.reservations, 6);
        assert_eq!(stats.releases, 6);
        assert_eq!(stats.dropped_releases, 3);
        assert_eq!(stats.in_flight, 0);
        let measurement = model.measurement(&a, ms(10));
        assert!(measurement.outstanding.abs() < f64::EPSILON);
        assert_eq!(measurement.in_flight, 0);
    }

    #[test]
    fn keys_are_endpoint_incarnations_and_latency_is_measured() {
        let model = model();
        let old = endpoint(1, 1);
        let new = endpoint(1, 2);
        let r = model.reserve(old, ms(100));
        r.release(ms(130), ModelOutcome::Clean(Feedback::default()), 0.0);
        assert_eq!(model.measurement(&old, ms(130)).latency, ms(30));
        // Same address, new incarnation: its own, fresh entry.
        assert_eq!(model.measurement(&new, ms(130)).latency, ms(1));
        // An unclean sample can only raise the latency; a clean one sets it.
        let r = model.reserve(old, ms(200));
        r.release(ms(210), ModelOutcome::Failed, 0.0);
        assert_eq!(model.measurement(&old, ms(210)).latency, ms(30));
        let r = model.reserve(old, ms(300));
        r.release(ms(350), ModelOutcome::Failed, 0.0);
        assert_eq!(model.measurement(&old, ms(350)).latency, ms(50));
        let r = model.reserve(old, ms(400));
        r.release(ms(405), ModelOutcome::Clean(Feedback::default()), 0.0);
        assert_eq!(model.measurement(&old, ms(405)).latency, ms(5));
    }

    #[test]
    fn penalties_weigh_outstanding_work() {
        let model = model();
        let a = endpoint(1, 1);
        let r = model.reserve(a, ms(0));
        r.release(
            ms(1),
            ModelOutcome::Clean(Feedback {
                penalty: Some(3.0),
                busy: Some(7),
            }),
            0.0,
        );
        let measurement = model.measurement(&a, ms(1));
        assert!((measurement.penalty - 3.0).abs() < 1e-9);
        assert_eq!(measurement.busy, Some((7, ms(1))));
        let r1 = model.reserve(a, ms(2));
        let r2 = model.reserve(a, ms(2));
        assert!((model.measurement(&a, ms(2)).outstanding - 6.0).abs() < 1e-9);
        // Penalty changes while in flight: each release removes what its
        // own reservation added.
        r1.release(
            ms(3),
            ModelOutcome::Clean(Feedback {
                penalty: Some(1.0),
                busy: None,
            }),
            0.0,
        );
        assert!((model.measurement(&a, ms(3)).outstanding - 3.0).abs() < 1e-9);
        drop(r2);
        assert!(model.measurement(&a, ms(3)).outstanding.abs() < f64::EPSILON);
        // Absurd penalties are bounded; nonsense ones ignored.
        let r = model.reserve(a, ms(4));
        r.release(
            ms(5),
            ModelOutcome::Clean(Feedback {
                penalty: Some(f64::INFINITY),
                busy: None,
            }),
            0.0,
        );
        assert!((model.measurement(&a, ms(5)).penalty - 1.0).abs() < 1e-9);
        let r = model.reserve(a, ms(6));
        r.release(
            ms(7),
            ModelOutcome::Clean(Feedback {
                penalty: Some(1e9),
                busy: None,
            }),
            0.0,
        );
        assert!((model.measurement(&a, ms(7)).penalty - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn overload_excludes_records_the_penalty_and_is_not_a_clean_sample() {
        let model = model();
        let a = endpoint(1, 1);
        let r = model.reserve(a, ms(0));
        r.release(ms(40), ModelOutcome::Clean(Feedback::default()), 0.0);
        let r = model.reserve(a, ms(100));
        r.release(
            ms(105),
            ModelOutcome::Overloaded(Feedback {
                penalty: Some(4.0),
                busy: None,
            }),
            0.0,
        );
        let measurement = model.measurement(&a, ms(105));
        assert_eq!(measurement.excluded_until, Some(ms(1105)));
        assert!((measurement.penalty - 4.0).abs() < 1e-9);
        assert_eq!(
            measurement.latency,
            ms(40),
            "a fast refusal is no clean sample"
        );
        // A second overload grows the backoff instead of resetting it; the
        // transport's refusal (no feedback) keeps the penalty.
        let r = model.reserve(a, ms(1200));
        r.release(ms(1200), ModelOutcome::Overloaded(Feedback::default()), 0.0);
        let measurement = model.measurement(&a, ms(1200));
        assert_eq!(measurement.excluded_until, Some(ms(3200)));
        assert!((measurement.penalty - 4.0).abs() < 1e-9);
        assert_eq!(model.stats().exclusions, 2);
    }

    #[test]
    fn exclusion_expires_and_its_backoff_grows_then_resets() {
        let model = model();
        let a = endpoint(1, 1);
        let behind = |at: u64| {
            let r = model.reserve(a, ms(at));
            r.release(ms(at), ModelOutcome::Behind, 0.0);
        };
        behind(0);
        assert_eq!(model.measurement(&a, ms(0)).excluded_until, Some(ms(1000)));
        assert_eq!(
            model.measurement(&a, ms(999)).excluded_until,
            Some(ms(1000))
        );
        // Expired: selectable again without any other event.
        assert_eq!(model.measurement(&a, ms(1000)).excluded_until, None);
        behind(1500);
        assert_eq!(
            model.measurement(&a, ms(1500)).excluded_until,
            Some(ms(3500))
        );
        behind(4000);
        behind(9000);
        behind(20_000);
        // Bounded by the maximum (8 s).
        assert_eq!(
            model.measurement(&a, ms(20_000)).excluded_until,
            Some(ms(28_000))
        );
        // Jitter only shortens it.
        let r = model.reserve(a, ms(40_000));
        r.release(ms(40_000), ModelOutcome::Behind, 1.0);
        let until = model
            .measurement(&a, ms(40_000))
            .excluded_until
            .expect("excluded");
        assert!(until >= ms(46_000) && until <= ms(48_000), "{until:?}");
        // A clean response resets the backoff.
        let r = model.reserve(a, ms(50_000));
        r.release(ms(50_001), ModelOutcome::Clean(Feedback::default()), 0.0);
        behind(60_000);
        assert_eq!(
            model.measurement(&a, ms(60_000)).excluded_until,
            Some(ms(61_000))
        );
        assert_eq!(model.stats().exclusions, 7);
    }

    #[test]
    fn hedge_budget_grows_is_consumed_and_exhausts() {
        let model = QueueModel::new(ModelConfig {
            hedge_budget: super::HedgeBudget {
                initial: 1.0,
                growth: 0.5,
                max: 2.0,
            },
            ..ModelConfig::default()
        })
        .expect("valid");
        assert!(model.try_take_copy());
        assert!(!model.try_take_copy(), "exhausted");
        model.first_response();
        assert!(!model.try_take_copy(), "half a unit is not a copy");
        model.first_response();
        assert!(model.try_take_copy());
        for _ in 0..10 {
            model.first_response();
        }
        let stats = model.stats();
        assert!((stats.hedge_budget - 2.0).abs() < 1e-9, "bounded");
        assert_eq!(stats.copies_granted, 2);
        assert_eq!(stats.copies_denied, 2);
        // The multiplier grows with copies, decays with first responses,
        // never below one.
        assert!(stats.multiplier >= 1.0);
        assert!(model.multiplier() < 1.02);
    }

    #[test]
    fn idle_endpoints_are_forgotten_busy_ones_kept() {
        let model = QueueModel::new(ModelConfig {
            max_tracked_endpoints: 2,
            ..ModelConfig::default()
        })
        .expect("valid");
        let busy = model.reserve(endpoint(1, 1), ms(0));
        let idle = model.reserve(endpoint(2, 1), ms(0));
        idle.release(ms(1), ModelOutcome::Failed, 0.0);
        let third = model.reserve(endpoint(3, 1), ms(2));
        assert_eq!(model.stats().endpoints, 2);
        assert_eq!(model.measurement(&endpoint(1, 1), ms(2)).in_flight, 1);
        drop((busy, third));
        assert_eq!(model.stats().in_flight, 0);
    }

    #[test]
    fn late_losers_are_bounded_and_release_once() {
        let model = QueueModel::new(ModelConfig {
            max_lagging: 2,
            ..ModelConfig::default()
        })
        .expect("valid");
        let a = endpoint(1, 1);
        for _ in 0..3 {
            let reservation = model.reserve(a, ms(0));
            let polled = Arc::new(AtomicUsize::new(0));
            let mut reservation = Some(reservation);
            model.push_lagging(Box::pin(futures::future::poll_fn(move |_| {
                // Completes on its second poll.
                if polled.fetch_add(1, Ordering::Relaxed).is_multiple_of(2) {
                    return Poll::Pending;
                }
                if let Some(reservation) = reservation.take() {
                    reservation.release(ms(4), ModelOutcome::Clean(Feedback::default()), 0.0);
                }
                Poll::Ready(LagEnd::Completed)
            })));
        }
        // The oldest was evicted and its reservation dropped.
        let stats = model.stats();
        assert_eq!(stats.lagging, 2);
        assert_eq!(stats.lagging_evicted, 1);
        assert_eq!(stats.releases, 1);
        let collector = model.collect_lagging();
        futures::pin_mut!(collector);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        for _ in 0..4 {
            assert!(collector.as_mut().poll(&mut cx).is_pending());
        }
        let stats = model.stats();
        assert_eq!(stats.lagging, 0);
        assert_eq!(stats.lagging_completed, 2);
        assert_eq!(stats.reservations, 3);
        assert_eq!(stats.releases, 3);
        assert_eq!(stats.in_flight, 0);
        assert_eq!(model.measurement(&a, ms(4)).latency, ms(4));
    }

    #[test]
    fn invalid_configurations_are_refused() {
        let bad_budget = ModelConfig {
            hedge_budget: super::HedgeBudget {
                initial: 5.0,
                growth: 0.1,
                max: 1.0,
            },
            ..ModelConfig::default()
        };
        assert!(QueueModel::new(bad_budget).is_err());
        let no_lagging = ModelConfig {
            max_lagging: 0,
            ..ModelConfig::default()
        };
        assert!(QueueModel::new(no_lagging).is_err());
    }
}
