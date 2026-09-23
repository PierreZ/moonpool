//! The balanced client: `FoundationDB`'s `loadBalance` over an explicit,
//! versioned alternative set, with separate retry and duplicate
//! permissions.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moonpool_core::{Providers, RandomProvider, TimeProvider};

use super::hooks::{
    AttemptEnd, AttemptEvent, AttemptKind, BalanceHooks, Candidate, DefaultHooks, QueueSelector,
    Selector, Verdict,
};
use super::locality::Distance;
use super::model::{LagEnd, ModelOutcome, QueueModel};
use super::policy::{BalanceConfig, BalancePolicy, HedgeTiming, Retry};
use super::set::{AlternativeSet, ReplaceError, SetStatus, SetVersion};
use crate::call::ServiceClient;
use crate::config::InvalidConfig;
use crate::endpoint::Endpoint;
use crate::error::{ErrorReason, Execution, RpcError};
use crate::failure::watch::Watch;
use crate::failure::{AddressState, EndpointState, FailureMonitor};
use crate::protocol::RpcMethod;
use crate::transport::RpcHandle;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("Mutex poisoned: prior task panicked")
}

/// How one attempt of a call ended, as the call saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// A reply the classifier accepted (the winner's, or one that arrived
    /// while the winner waited for its comparison copy).
    Replied,
    /// A reply the classifier declined (behind or overloaded): the handler
    /// ran and, by the application's own account, had no effect.
    Declined,
    /// An error, with what it proves.
    Failed(RpcError),
    /// Still in flight when the call finished: handed to the model's
    /// late-loser collection. May have executed.
    Lagging,
}

impl AttemptOutcome {
    /// What the outcome proves about this attempt's execution.
    #[must_use]
    pub fn execution(&self) -> Execution {
        match self {
            Self::Replied | Self::Declined => Execution::Executed,
            Self::Failed(error) => error.execution(),
            Self::Lagging => Execution::MaybeExecuted,
        }
    }
}

/// One attempt of a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    /// Index of the alternative in the call's set.
    pub alternative: usize,
    /// The endpoint incarnation the attempt went to (the set's reference,
    /// never a refreshed one).
    pub endpoint: Endpoint,
    /// Why it was started.
    pub kind: AttemptKind,
    /// How it ended for the call.
    pub outcome: AttemptOutcome,
}

/// A successful balanced call.
#[derive(Debug, Clone, PartialEq)]
pub struct Balanced<R> {
    /// The winner's reply: the only one that completes the caller.
    pub reply: R,
    /// Index of the winning alternative in the call's set.
    pub alternative: usize,
    /// The winner's endpoint incarnation.
    pub endpoint: Endpoint,
    /// Version of the set the call ran on.
    pub version: SetVersion,
    /// Every attempt the call started, in the order they ended; attempts
    /// still in flight are [`AttemptOutcome::Lagging`].
    pub attempts: Vec<AttemptRecord>,
}

/// Why a balanced call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BalanceFailure {
    /// The set is empty.
    NoAlternatives,
    /// Every alternative is permanently gone (endpoint not found or a
    /// stale incarnation): replace the set. Never refreshed silently.
    StaleAlternatives,
    /// No alternative was reachable for the whole wait bound
    /// (`all_alternatives_failed`).
    AllAlternativesFailed,
    /// The attempt error that ended the call: not retryable (a contract
    /// error, an execution without a usable reply), ambiguous without
    /// [`Retry::AfterAmbiguous`], or the last one when the attempt bound
    /// ran out.
    Rejected(RpcError),
    /// Every attempt was declined by the classifier until the attempt
    /// bound ran out.
    Declined,
    /// The comparison hook refused the winner.
    Comparison(String),
    /// The local runtime is gone.
    Shutdown,
}

impl std::fmt::Display for BalanceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAlternatives => f.write_str("the alternative set is empty"),
            Self::StaleAlternatives => {
                f.write_str("every alternative is permanently gone; the set must be replaced")
            }
            Self::AllAlternativesFailed => f.write_str("no alternative was reachable"),
            Self::Rejected(error) => write!(f, "attempt failed: {error}"),
            Self::Declined => f.write_str("every attempt was declined"),
            Self::Comparison(detail) => write!(f, "comparison failed: {detail}"),
            Self::Shutdown => f.write_str("the RPC runtime has shut down"),
        }
    }
}

/// A failed balanced call: why, what the attempts together prove about
/// execution, and each attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceError {
    failure: BalanceFailure,
    execution: Execution,
    version: SetVersion,
    attempts: Vec<AttemptRecord>,
}

impl BalanceError {
    fn new(failure: BalanceFailure, version: SetVersion, attempts: Vec<AttemptRecord>) -> Self {
        let execution = attempts
            .iter()
            .map(|record| record.outcome.execution())
            .max()
            .unwrap_or(Execution::NotAdmitted);
        Self {
            failure,
            execution,
            version,
            attempts,
        }
    }

    /// Why the call failed.
    #[must_use]
    pub fn failure(&self) -> &BalanceFailure {
        &self.failure
    }

    /// The strongest execution knowledge over every attempt: never
    /// [`Execution::NotAdmitted`] if any attempt may have run.
    #[must_use]
    pub fn execution(&self) -> Execution {
        self.execution
    }

    /// Version of the set the call ran on.
    #[must_use]
    pub fn version(&self) -> SetVersion {
        self.version
    }

    /// Every attempt, each with its own execution knowledge.
    #[must_use]
    pub fn attempts(&self) -> &[AttemptRecord] {
        &self.attempts
    }
}

impl std::fmt::Display for BalanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({:?}, {} attempts, set {})",
            self.failure,
            self.execution,
            self.attempts.len(),
            self.version
        )
    }
}

impl std::error::Error for BalanceError {}

/// The installed set with what the client derived from it.
struct Snapshot<P: Providers, M: RpcMethod> {
    set: AlternativeSet<M>,
    distances: Vec<Distance>,
    clients: Vec<ServiceClient<P, M>>,
}

struct Installed<P: Providers, M: RpcMethod> {
    snapshot: Arc<Snapshot<P, M>>,
    stale: bool,
    all_failed: bool,
}

/// Balances calls of one method over an explicit alternative set.
///
/// Cheap to clone; clones share the installed set, the model and the
/// hooks. The client holds the runtime weakly: calls fail with
/// [`BalanceFailure::Shutdown`] once it is gone.
///
/// Each call ([`call`](Self::call)) snapshots the installed set, ranks it
/// with the [`Selector`] over the [`QueueModel`] and the failure monitor,
/// and sends attempts under its [`BalancePolicy`]. Only the winner
/// completes the caller; attempts still in flight when the call returns or
/// is dropped become late losers that update the model when they end.
pub struct BalancedClient<P: Providers, M: RpcMethod> {
    rpc: RpcHandle<P>,
    model: QueueModel,
    config: Arc<BalanceConfig>,
    hooks: Arc<dyn BalanceHooks<M>>,
    selector: Arc<dyn Selector>,
    installed: Arc<Mutex<Installed<P, M>>>,
    watch: Arc<Watch>,
}

impl<P: Providers, M: RpcMethod> Clone for BalancedClient<P, M> {
    fn clone(&self) -> Self {
        Self {
            rpc: self.rpc.clone(),
            model: self.model.clone(),
            config: Arc::clone(&self.config),
            hooks: Arc::clone(&self.hooks),
            selector: Arc::clone(&self.selector),
            installed: Arc::clone(&self.installed),
            watch: Arc::clone(&self.watch),
        }
    }
}

impl<P: Providers, M: RpcMethod> std::fmt::Debug for BalancedClient<P, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BalancedClient")
            .field("method", &M::NAME)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// What the choice step found.
enum Choice {
    /// Every alternative is permanently gone.
    Dead,
    /// None is reachable right now.
    Unreachable,
    /// Reachable alternatives, best first.
    Ordered(Vec<usize>),
}

/// What woke a waiting call.
enum Event<R> {
    Landed(Landed<R>),
    HedgeDue,
}

/// A finished attempt.
struct Landed<R> {
    id: u64,
    outcome: Result<R, RpcError>,
    verdict: Option<Verdict>,
}

type Flight<R> = Pin<Box<dyn Future<Output = Landed<R>> + Send>>;

/// Reports an attempt's end exactly once: when it lands, or when it is
/// dropped unfinished.
struct EndGuard<M: RpcMethod> {
    hooks: Arc<dyn BalanceHooks<M>>,
    alternative: usize,
    endpoint: Endpoint,
    kind: AttemptKind,
    late: Arc<AtomicBool>,
    fired: bool,
}

impl<M: RpcMethod> EndGuard<M> {
    fn fire(&mut self, end: AttemptEnd) {
        if self.fired {
            return;
        }
        self.fired = true;
        self.hooks.observe(&AttemptEvent::Ended {
            alternative: self.alternative,
            endpoint: self.endpoint,
            kind: self.kind,
            end,
            late: self.late.load(Ordering::Acquire),
        });
    }
}

impl<M: RpcMethod> Drop for EndGuard<M> {
    fn drop(&mut self) {
        self.fire(AttemptEnd::Dropped);
    }
}

/// An attempt in flight, as the call tracks it.
#[derive(Clone, Copy)]
struct Active {
    alternative: usize,
    endpoint: Endpoint,
    kind: AttemptKind,
}

/// A call's attempts. Whatever is still in flight when the call finishes
/// or is dropped becomes a late loser of the model.
struct Flights<P: Providers, R: Send + 'static> {
    pending: FuturesUnordered<Flight<R>>,
    active: BTreeMap<u64, Active>,
    next_id: u64,
    late: Arc<AtomicBool>,
    model: QueueModel,
    time: P::Time,
}

impl<P: Providers, R: Send + 'static> Flights<P, R> {
    fn primaries(&self) -> usize {
        self.active
            .values()
            .filter(|active| active.kind != AttemptKind::Comparison)
            .count()
    }

    fn in_flight(&self, alternative: usize) -> bool {
        self.active
            .values()
            .any(|active| active.alternative == alternative)
    }

    /// Records for the attempts still in flight.
    fn lagging_records(&self) -> Vec<AttemptRecord> {
        self.active
            .values()
            .map(|active| AttemptRecord {
                alternative: active.alternative,
                endpoint: active.endpoint,
                kind: active.kind,
                outcome: AttemptOutcome::Lagging,
            })
            .collect()
    }
}

impl<P: Providers, R: Send + 'static> Drop for Flights<P, R> {
    fn drop(&mut self) {
        self.late.store(true, Ordering::Release);
        let timeout = self.model.config().lagging_timeout;
        for flight in std::mem::take(&mut self.pending) {
            let time = self.time.clone();
            self.model.push_lagging(Box::pin(async move {
                match time.timeout(timeout, flight).await {
                    Ok(_) => LagEnd::Completed,
                    Err(_) => LagEnd::Expired,
                }
            }));
        }
    }
}

/// Errors after which no other alternative is tried: the request or the
/// set is wrong, or the local runtime is gone.
fn is_fatal(error: &RpcError) -> bool {
    matches!(
        error.reason(),
        ErrorReason::MethodMismatch { .. }
            | ErrorReason::InvalidReference(_)
            | ErrorReason::InterfaceMismatch { .. }
            | ErrorReason::SchemaMismatch { .. }
            | ErrorReason::CodecMismatch { .. }
            | ErrorReason::FrameTooLarge { .. }
            | ErrorReason::Encode(_)
            | ErrorReason::MalformedRequest
            | ErrorReason::ReplyTooLarge
            | ErrorReason::ReplyEncodeFailed
            | ErrorReason::MalformedReply(_)
            | ErrorReason::Shutdown
            | ErrorReason::NotListening
            | ErrorReason::AlreadyRegistered
    )
}

/// The hedge delay (`loadBalance`'s `secondDelay`): zero when the first
/// choice is already much slower than the second.
fn hedge_delay(
    timing: &HedgeTiming,
    multiplier: f64,
    first: Duration,
    second: Duration,
) -> Duration {
    let delay = second.mul_f64(multiplier.max(1.0)) + timing.base;
    if first.as_secs_f64() > timing.instant_factor.max(0.0) * delay.as_secs_f64() {
        Duration::ZERO
    } else {
        delay
    }
}

/// Per-call environment taken from the live runtime.
struct Env<P: Providers> {
    time: P::Time,
    random: P::Random,
    monitor: FailureMonitor<P>,
}

impl<P: Providers, M: RpcMethod> BalancedClient<P, M> {
    /// A client over `set`, bound to `rpc`, measuring into `model`, with
    /// the neutral hooks and the queue-model selector.
    ///
    /// # Errors
    ///
    /// An invalid [`BalanceConfig`].
    pub fn new(
        rpc: &RpcHandle<P>,
        set: AlternativeSet<M>,
        model: QueueModel,
        config: BalanceConfig,
    ) -> Result<Self, InvalidConfig> {
        config.validate()?;
        let snapshot = Arc::new(snapshot_of(rpc, &config, set));
        Ok(Self {
            rpc: rpc.clone(),
            model,
            config: Arc::new(config),
            hooks: Arc::new(DefaultHooks),
            selector: Arc::new(QueueSelector::default()),
            installed: Arc::new(Mutex::new(Installed {
                snapshot,
                stale: false,
                all_failed: false,
            })),
            watch: Arc::new(Watch::default()),
        })
    }

    /// Use `hooks` for classification, comparison and observation.
    #[must_use]
    pub fn with_hooks(mut self, hooks: impl BalanceHooks<M>) -> Self {
        self.hooks = Arc::new(hooks);
        self
    }

    /// Use `selector` to order alternatives.
    #[must_use]
    pub fn with_selector(mut self, selector: impl Selector) -> Self {
        self.selector = Arc::new(selector);
        self
    }

    /// The model this client measures into.
    #[must_use]
    pub fn model(&self) -> &QueueModel {
        &self.model
    }

    /// The installed set.
    #[must_use]
    pub fn alternatives(&self) -> AlternativeSet<M> {
        lock(&self.installed).snapshot.set.clone()
    }

    /// What the client knows about its installed set.
    #[must_use]
    pub fn status(&self) -> SetStatus {
        let installed = lock(&self.installed);
        SetStatus {
            version: installed.snapshot.set.version(),
            alternatives: installed.snapshot.set.len(),
            stale: installed.stale,
            all_failed: installed.all_failed,
        }
    }

    /// Resolves at the next change of [`status`](Self::status) after this
    /// call (a stale or all-failed verdict, a recovery, a replacement).
    pub fn on_status_change(&self) -> impl Future<Output = ()> + Send + 'static {
        let watch = Arc::clone(&self.watch);
        let since = watch.version();
        async move {
            let _ = watch.changed(since).await;
        }
    }

    /// Install a newer set. Calls already running keep the set they
    /// started with, and their attempts keep their original references.
    ///
    /// # Errors
    ///
    /// [`ReplaceError::NotNewer`] unless `set`'s version is strictly
    /// greater than the installed one.
    pub fn replace(&self, set: AlternativeSet<M>) -> Result<(), ReplaceError> {
        let mut installed = lock(&self.installed);
        let current = installed.snapshot.set.version();
        if set.version() <= current {
            return Err(ReplaceError::NotNewer {
                installed: current,
                offered: set.version(),
            });
        }
        installed.snapshot = Arc::new(snapshot_of(&self.rpc, &self.config, set));
        installed.stale = false;
        installed.all_failed = false;
        drop(installed);
        self.watch.notify();
        Ok(())
    }

    fn mark(&self, version: SetVersion, stale: Option<bool>, all_failed: Option<bool>) {
        let mut installed = lock(&self.installed);
        if installed.snapshot.set.version() != version {
            return;
        }
        let mut changed = false;
        if let Some(stale) = stale
            && installed.stale != stale
        {
            installed.stale = stale;
            changed = true;
        }
        if let Some(all_failed) = all_failed
            && installed.all_failed != all_failed
        {
            installed.all_failed = all_failed;
            changed = true;
        }
        drop(installed);
        if changed {
            self.watch.notify();
        }
    }

    /// Call the method on the best alternative under `policy`.
    ///
    /// Attempts go to alternatives in the [`Selector`]'s order. After an
    /// attempt that provably never reached a handler, or that the
    /// classifier declared without effect, the call fails over to the next
    /// alternative; after an ambiguous one only with
    /// [`Retry::AfterAmbiguous`]. Concurrent copies (hedges, comparison
    /// copies) need [`Duplicates::Permitted`](super::Duplicates::Permitted)
    /// and a unit of the model's hedge budget each. Only the winner's reply
    /// completes the call.
    ///
    /// Dropping the future cancels the call: attempts in flight become
    /// late losers (they still update and release the model), and nothing
    /// sent is retracted.
    ///
    /// # Errors
    ///
    /// A [`BalanceError`] whose execution knowledge covers every attempt.
    pub async fn call(
        &self,
        request: M::Request,
        policy: &BalancePolicy,
    ) -> Result<Balanced<M::Reply>, BalanceError> {
        let snapshot = Arc::clone(&lock(&self.installed).snapshot);
        let version = snapshot.set.version();
        if snapshot.set.is_empty() {
            return Err(BalanceError::new(
                BalanceFailure::NoAlternatives,
                version,
                Vec::new(),
            ));
        }
        let Some(env) = self.env() else {
            return Err(BalanceError::new(
                BalanceFailure::Shutdown,
                version,
                Vec::new(),
            ));
        };
        // `Wire` types are `Send` but not necessarily `Sync`; behind a
        // mutex the request can be borrowed across awaits and the call
        // future stays `Send`.
        let request = Mutex::new(request);
        let call = Call {
            client: self,
            snapshot: &snapshot,
            env: &env,
            request: &request,
            policy,
        };
        let mut progress = Progress {
            flights: Flights {
                pending: FuturesUnordered::new(),
                active: BTreeMap::new(),
                next_id: 0,
                late: Arc::new(AtomicBool::new(false)),
                model: self.model.clone(),
                time: env.time.clone(),
            },
            records: Vec::new(),
            dead: BTreeSet::new(),
            tried: BTreeSet::new(),
            sequential: 0,
            copies: 0,
            backoff: Duration::ZERO,
            hedge: None,
            comparison: None,
            last: None,
            blocked: None,
            probed: false,
        };
        let outcome = call.run(&mut progress).await;
        let mut records = std::mem::take(&mut progress.records);
        records.extend(progress.flights.lagging_records());
        // Dropping the progress hands attempts still in flight to the
        // model's late-loser collection.
        drop(progress);
        match outcome {
            Ok((reply, alternative)) => {
                self.mark(version, None, Some(false));
                Ok(Balanced {
                    reply,
                    alternative,
                    endpoint: snapshot.set.alternatives()[alternative].target.endpoint(),
                    version,
                    attempts: records,
                })
            }
            Err(failure) => {
                match failure {
                    BalanceFailure::StaleAlternatives => self.mark(version, Some(true), None),
                    BalanceFailure::AllAlternativesFailed => {
                        self.mark(version, None, Some(true));
                    }
                    _ => {}
                }
                tracing::debug!(method = M::NAME, %failure, "rpc balanced call failed");
                Err(BalanceError::new(failure, version, records))
            }
        }
    }

    fn env(&self) -> Option<Env<P>> {
        let shared = self.rpc.upgrade()?;
        Some(Env {
            time: shared.time().clone(),
            random: shared.random().clone(),
            monitor: self.rpc.failure_monitor()?,
        })
    }
}

fn snapshot_of<P: Providers, M: RpcMethod>(
    rpc: &RpcHandle<P>,
    config: &BalanceConfig,
    set: AlternativeSet<M>,
) -> Snapshot<P, M> {
    let distances = set
        .alternatives()
        .iter()
        .map(|alternative| config.locality.distance_to(&alternative.locality))
        .collect();
    let clients = set
        .alternatives()
        .iter()
        .map(|alternative| alternative.target.bind(rpc))
        .collect();
    Snapshot {
        set,
        distances,
        clients,
    }
}

/// The comparison copy's outcome, once it arrived.
struct Comparison<R> {
    outcome: Option<Result<R, RpcError>>,
}

type Timer = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A call's progress: its attempts and what it learned.
struct Progress<P: Providers, R: Send + 'static> {
    flights: Flights<P, R>,
    records: Vec<AttemptRecord>,
    /// Alternatives known gone for this call.
    dead: BTreeSet<usize>,
    /// Alternatives tried in the current round (in flight included).
    tried: BTreeSet<usize>,
    sequential: u32,
    copies: u32,
    backoff: Duration,
    hedge: Option<Timer>,
    comparison: Option<Comparison<R>>,
    last: Option<BalanceFailure>,
    /// An ambiguous attempt without retry permission: nothing new starts.
    blocked: Option<RpcError>,
    /// Alternatives believed unreachable were already probed.
    probed: bool,
}

/// One running call.
struct Call<'a, P: Providers, M: RpcMethod> {
    client: &'a BalancedClient<P, M>,
    snapshot: &'a Snapshot<P, M>,
    env: &'a Env<P>,
    request: &'a Mutex<M::Request>,
    policy: &'a BalancePolicy,
}

impl<P: Providers, M: RpcMethod> Call<'_, P, M> {
    /// Rank the alternatives not yet `dead` for this call; with `probe`,
    /// alternatives whose address is believed failed are usable too (after
    /// the reachable ones).
    fn choose(&self, dead: &mut BTreeSet<usize>, probe: bool) -> Choice {
        let now = self.env.time.now();
        let model = &self.client.model;
        let monitor = &self.env.monitor;
        let mut candidates = Vec::with_capacity(self.snapshot.set.len());
        for (index, alternative) in self.snapshot.set.alternatives().iter().enumerate() {
            if dead.contains(&index) {
                continue;
            }
            let endpoint = alternative.target.endpoint();
            let state = monitor.endpoint_state(&endpoint);
            if state.is_permanent() {
                dead.insert(index);
                continue;
            }
            let measurement = model.measurement(&endpoint, now);
            candidates.push(Candidate {
                index,
                distance: self.snapshot.distances[index],
                available: state == EndpointState::Available
                    && monitor.address_state(endpoint.address()) == AddressState::Available,
                excluded: measurement.excluded_until.is_some(),
                penalty: measurement.penalty,
                outstanding: measurement.outstanding,
                latency: measurement.latency,
                busy: measurement
                    .busy
                    .map(|(score, at)| (score, now.saturating_sub(at))),
            });
        }
        if candidates.is_empty() {
            return Choice::Dead;
        }
        let random = self.env.random.clone();
        let mut draw = move |n: u64| random.random_range(0..n.max(1));
        let proposed = self.client.selector.order(&candidates, &mut draw);
        let usable: BTreeSet<usize> = candidates
            .iter()
            .filter(|candidate| candidate.available || probe)
            .map(|candidate| candidate.index)
            .collect();
        let mut order = Vec::with_capacity(usable.len());
        let mut seen = BTreeSet::new();
        for index in proposed {
            if usable.contains(&index) && seen.insert(index) {
                order.push(index);
            }
        }
        // No selector hides a usable alternative.
        for index in usable {
            if seen.insert(index) {
                order.push(index);
            }
        }
        if order.is_empty() {
            Choice::Unreachable
        } else {
            Choice::Ordered(order)
        }
    }

    /// Wait (bounded, jittered) until some alternative is reachable;
    /// `None` on shutdown.
    async fn wait_reachable(&self, dead: &mut BTreeSet<usize>) -> Option<Choice> {
        let config = &self.client.config;
        let bound = self.env.time.now()
            + config
                .round_backoff
                .jittered(config.all_failed_wait, self.env.random.random_ratio());
        loop {
            // Register before checking: no change is lost.
            let change = self.env.monitor.on_change();
            match self.choose(dead, false) {
                Choice::Unreachable => {}
                other => return Some(other),
            }
            let Some(left) = bound
                .checked_sub(self.env.time.now())
                .filter(|left| !left.is_zero())
            else {
                return Some(Choice::Unreachable);
            };
            match self.env.time.timeout(left, change).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return None,
                Err(_) => return Some(Choice::Unreachable),
            }
        }
    }

    /// Start one attempt: reserve, report, send.
    fn start(&self, flights: &mut Flights<P, M::Reply>, alternative: usize, kind: AttemptKind) {
        let client = &self.snapshot.clients[alternative];
        let endpoint = client.target().endpoint();
        let hooks = Arc::clone(&self.client.hooks);
        let reservation = self.client.model.reserve(endpoint, self.env.time.now());
        hooks.observe(&AttemptEvent::Started {
            alternative,
            endpoint,
            kind,
        });
        tracing::debug!(method = M::NAME, alternative, ?kind, %endpoint, "rpc balance attempt started");
        let started = client.attempt(&lock(self.request));
        let mut guard = EndGuard {
            hooks: Arc::clone(&hooks),
            alternative,
            endpoint,
            kind,
            late: Arc::clone(&flights.late),
            fired: false,
        };
        let time = self.env.time.clone();
        let random = self.env.random.clone();
        let timeout = self.policy.attempt_timeout;
        let id = flights.next_id;
        flights.next_id += 1;
        flights.pending.push(Box::pin(async move {
            let outcome = match started {
                Err(error) => Err(error),
                Ok(attempt) => match timeout {
                    Some(timeout) => attempt.within(timeout).await,
                    None => attempt.await,
                },
            };
            let verdict = outcome.as_ref().ok().map(|reply| hooks.classify(reply));
            let model_outcome = match (&outcome, verdict) {
                (Ok(_), Some(Verdict::Accept(feedback) | Verdict::Overloaded(feedback))) => {
                    ModelOutcome::Clean(feedback)
                }
                (Ok(_), Some(Verdict::Behind) | None) => ModelOutcome::Behind,
                (Err(error), _) if *error.reason() == ErrorReason::Overloaded => {
                    ModelOutcome::Behind
                }
                (Err(_), _) => ModelOutcome::Failed,
            };
            reservation.release(time.now(), model_outcome, random.random_ratio());
            guard.fire(match &outcome {
                Ok(_) => AttemptEnd::Replied,
                Err(error) => AttemptEnd::Failed(error.clone()),
            });
            Landed {
                id,
                outcome,
                verdict,
            }
        }));
        flights.active.insert(
            id,
            Active {
                alternative,
                endpoint,
                kind,
            },
        );
    }

    async fn run(
        &self,
        progress: &mut Progress<P, M::Reply>,
    ) -> Result<(M::Reply, usize), BalanceFailure> {
        loop {
            if progress.flights.primaries() == 0 {
                self.next_attempt(progress).await?;
            }
            match next_event(&mut progress.flights.pending, &mut progress.hedge).await {
                None => {}
                Some(Event::HedgeDue) => self.hedge(progress),
                Some(Event::Landed(landed)) => {
                    if let Some(winner) = self.land(progress, landed).await? {
                        return Ok(winner);
                    }
                }
            }
        }
    }

    /// Start the next sequential attempt, or end the call.
    async fn next_attempt(
        &self,
        progress: &mut Progress<P, M::Reply>,
    ) -> Result<(), BalanceFailure> {
        loop {
            if let Some(blocked) = progress.blocked.take() {
                // An ambiguous attempt without retry permission.
                return Err(BalanceFailure::Rejected(blocked));
            }
            if progress.sequential >= self.policy.max_attempts.max(1) {
                return Err(progress
                    .last
                    .take()
                    .unwrap_or(BalanceFailure::AllAlternativesFailed));
            }
            let order = match self.choose(&mut progress.dead, false) {
                Choice::Dead => return Err(BalanceFailure::StaleAlternatives),
                Choice::Ordered(order) => order,
                Choice::Unreachable => match self.wait_reachable(&mut progress.dead).await {
                    None => return Err(BalanceFailure::Shutdown),
                    Some(Choice::Dead) => return Err(BalanceFailure::StaleAlternatives),
                    Some(Choice::Ordered(order)) => order,
                    Some(Choice::Unreachable) if progress.probed => {
                        return Err(BalanceFailure::AllAlternativesFailed);
                    }
                    Some(Choice::Unreachable) => {
                        // A failed address is an observation that only a
                        // new connection can change: probe with one
                        // attempt before giving up, so no belief excludes
                        // every alternative for good.
                        progress.probed = true;
                        match self.choose(&mut progress.dead, true) {
                            Choice::Ordered(order) => order,
                            Choice::Dead => return Err(BalanceFailure::StaleAlternatives),
                            Choice::Unreachable => {
                                return Err(BalanceFailure::AllAlternativesFailed);
                            }
                        }
                    }
                },
            };
            let Some(&target) = order.iter().find(|index| !progress.tried.contains(index)) else {
                // Every reachable alternative was tried: a new round,
                // after a growing, jittered pause.
                let backoff = &self.client.config.round_backoff;
                progress.tried.clear();
                progress.backoff = backoff.next(progress.backoff);
                let pause = backoff.jittered(progress.backoff, self.env.random.random_ratio());
                if self.env.time.sleep(pause).await.is_err() {
                    return Err(BalanceFailure::Shutdown);
                }
                continue;
            };
            let kind = if progress.sequential == 0 {
                AttemptKind::First
            } else {
                AttemptKind::Retry
            };
            progress.sequential += 1;
            progress.tried.insert(target);
            self.start(&mut progress.flights, target, kind);
            if kind == AttemptKind::First {
                self.arm_copies(progress, &order, target);
            }
            return Ok(());
        }
    }

    /// For a first attempt: start a comparison copy if the hooks ask for
    /// one, and arm the hedge timer.
    fn arm_copies(&self, progress: &mut Progress<P, M::Reply>, order: &[usize], target: usize) {
        let Some(copies) = self.policy.duplicates.policy() else {
            return;
        };
        let model = &self.client.model;
        let second = order.iter().copied().find(|index| *index != target);
        if progress.copies < copies.max_copies
            && let Some(second) = second
            && self.client.hooks.wants_comparison(&lock(self.request))
            && model.try_take_copy()
        {
            progress.copies += 1;
            progress.tried.insert(second);
            self.start(&mut progress.flights, second, AttemptKind::Comparison);
            progress.comparison = Some(Comparison { outcome: None });
        }
        if let Some(timing) = copies.hedge
            && progress.copies < copies.max_copies
            && order.iter().any(|index| !progress.tried.contains(index))
        {
            let now = self.env.time.now();
            let latency = |index: usize| {
                model
                    .measurement(
                        &self.snapshot.set.alternatives()[index].target.endpoint(),
                        now,
                    )
                    .latency
            };
            let next = order
                .iter()
                .copied()
                .find(|index| !progress.tried.contains(index))
                .unwrap_or(target);
            let delay = hedge_delay(&timing, model.multiplier(), latency(target), latency(next));
            let time = self.env.time.clone();
            progress.hedge = Some(Box::pin(async move {
                let _ = time.sleep(delay).await;
            }));
        }
    }

    /// The hedge timer fired: send a concurrent copy if still permitted
    /// and the budget grants one.
    fn hedge(&self, progress: &mut Progress<P, M::Reply>) {
        let Some(copies) = self.policy.duplicates.policy() else {
            return;
        };
        if progress.copies >= copies.max_copies || progress.blocked.is_some() {
            return;
        }
        let target = match self.choose(&mut progress.dead, false) {
            Choice::Ordered(order) => order.into_iter().find(|index| {
                !progress.flights.in_flight(*index) && !progress.tried.contains(index)
            }),
            Choice::Dead | Choice::Unreachable => None,
        };
        if let Some(target) = target
            && self.client.model.try_take_copy()
        {
            progress.copies += 1;
            progress.tried.insert(target);
            self.start(&mut progress.flights, target, AttemptKind::Hedge);
        }
    }

    /// Handle one landed attempt; `Some` is the winner.
    async fn land(
        &self,
        progress: &mut Progress<P, M::Reply>,
        landed: Landed<M::Reply>,
    ) -> Result<Option<(M::Reply, usize)>, BalanceFailure> {
        let Some(active) = progress.flights.active.remove(&landed.id) else {
            return Ok(None);
        };
        if active.kind == AttemptKind::First {
            self.client.model.first_response();
        }
        if active.kind == AttemptKind::Comparison {
            progress.records.push(active.record(&landed));
            if let Some(comparison) = progress.comparison.as_mut() {
                comparison.outcome = Some(landed.outcome);
            }
            return Ok(None);
        }
        progress.records.push(active.record(&landed));
        match (landed.outcome, landed.verdict) {
            (Ok(reply), Some(Verdict::Accept(_))) => {
                progress.hedge = None;
                if progress.comparison.is_some() {
                    self.await_comparison(progress).await;
                    let copy = progress
                        .comparison
                        .as_ref()
                        .and_then(|comparison| comparison.outcome.as_ref());
                    self.client
                        .hooks
                        .compare(&reply, copy)
                        .map_err(BalanceFailure::Comparison)?;
                }
                Ok(Some((reply, active.alternative)))
            }
            (Ok(_), _) => {
                // Declined without effect: another alternative may serve.
                progress.last = Some(BalanceFailure::Declined);
                Ok(None)
            }
            (Err(error), _) => {
                tracing::debug!(method = M::NAME, alternative = active.alternative, %error, "rpc balance attempt failed");
                if is_fatal(&error) || error.execution() == Execution::Executed {
                    return Err(BalanceFailure::Rejected(error));
                }
                if error.is_terminal_for_reference() {
                    progress.dead.insert(active.alternative);
                }
                if error.execution() == Execution::MaybeExecuted
                    && self.policy.retry != Retry::AfterAmbiguous
                {
                    // Copies already in flight may still win; nothing new
                    // starts.
                    progress.hedge = None;
                    if progress.blocked.is_none() {
                        progress.blocked = Some(error.clone());
                    }
                }
                progress.last = Some(BalanceFailure::Rejected(error));
                Ok(None)
            }
        }
    }

    /// Wait (bounded) for the comparison copy's outcome. Holds no
    /// reference to the winner across the wait, so the call future needs
    /// only `Send` replies.
    async fn await_comparison(&self, progress: &mut Progress<P, M::Reply>) {
        let within = self
            .policy
            .duplicates
            .policy()
            .map_or(Duration::ZERO, |copies| copies.compare_within);
        let deadline = self.env.time.now() + within;
        while progress
            .comparison
            .as_ref()
            .is_some_and(|comparison| comparison.outcome.is_none())
        {
            let Some(left) = deadline
                .checked_sub(self.env.time.now())
                .filter(|left| !left.is_zero())
            else {
                break;
            };
            let mut no_timer = None;
            let next = next_event(&mut progress.flights.pending, &mut no_timer);
            let Ok(Some(Event::Landed(landed))) = self.env.time.timeout(left, next).await else {
                break;
            };
            let Some(active) = progress.flights.active.remove(&landed.id) else {
                continue;
            };
            progress.records.push(active.record(&landed));
            if active.kind == AttemptKind::Comparison
                && let Some(comparison) = progress.comparison.as_mut()
            {
                comparison.outcome = Some(landed.outcome);
            }
        }
    }
}

impl Active {
    fn record<R>(&self, landed: &Landed<R>) -> AttemptRecord {
        AttemptRecord {
            alternative: self.alternative,
            endpoint: self.endpoint,
            kind: self.kind,
            outcome: match (&landed.outcome, landed.verdict) {
                (Ok(_), Some(Verdict::Accept(_))) => AttemptOutcome::Replied,
                (Ok(_), _) => AttemptOutcome::Declined,
                (Err(error), _) => AttemptOutcome::Failed(error.clone()),
            },
        }
    }
}

/// The next landed attempt or the hedge timer, landings first; `None`
/// when there is nothing to wait for.
async fn next_event<R>(
    flights: &mut FuturesUnordered<Flight<R>>,
    hedge: &mut Option<Timer>,
) -> Option<Event<R>> {
    if flights.is_empty() && hedge.is_none() {
        return None;
    }
    futures::future::poll_fn(|cx| {
        if !flights.is_empty()
            && let Poll::Ready(Some(landed)) = flights.poll_next_unpin(cx)
        {
            return Poll::Ready(Some(Event::Landed(landed)));
        }
        if let Some(timer) = hedge.as_mut()
            && timer.as_mut().poll(cx).is_ready()
        {
            *hedge = None;
            return Poll::Ready(Some(Event::HedgeDue));
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use super::{
        AttemptOutcome, AttemptRecord, BalanceError, BalanceFailure, hedge_delay, is_fatal,
    };
    use crate::balance::hooks::AttemptKind;
    use crate::balance::policy::HedgeTiming;
    use crate::balance::set::SetVersion;
    use crate::endpoint::{Endpoint, EndpointToken, Incarnation};
    use crate::error::{ErrorReason, Execution, RpcError};

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn hedge_delay_follows_the_second_choice_and_fires_at_once_when_the_first_is_slow() {
        let timing = HedgeTiming {
            base: ms(1),
            instant_factor: 2.0,
        };
        // multiplier × second + base.
        assert_eq!(hedge_delay(&timing, 1.0, ms(5), ms(10)), ms(11));
        assert_eq!(hedge_delay(&timing, 1.5, ms(5), ms(10)), ms(16));
        // The first choice is more than twice the delay: hedge now.
        assert_eq!(hedge_delay(&timing, 1.0, ms(23), ms(10)), Duration::ZERO);
        assert_eq!(hedge_delay(&timing, 1.0, ms(22), ms(10)), ms(11));
        // A multiplier below one never shortens it.
        assert_eq!(hedge_delay(&timing, 0.1, ms(1), ms(10)), ms(11));
    }

    #[test]
    fn contract_errors_stop_the_call_and_availability_errors_do_not() {
        let refused = RpcError::not_admitted;
        assert!(is_fatal(&refused(ErrorReason::MalformedRequest)));
        assert!(is_fatal(&refused(ErrorReason::Shutdown)));
        assert!(!is_fatal(&refused(ErrorReason::EndpointNotFound)));
        assert!(!is_fatal(&refused(ErrorReason::StaleIncarnation)));
        assert!(!is_fatal(&refused(ErrorReason::Overloaded)));
        assert!(!is_fatal(&refused(ErrorReason::ConnectFailed(
            "refused".into()
        ))));
        assert!(!is_fatal(&RpcError::new(
            ErrorReason::Disconnected,
            Execution::MaybeExecuted
        )));
    }

    #[test]
    fn a_call_future_is_send_even_when_the_request_is_not_sync() {
        use std::cell::Cell;

        use moonpool_core::TokioProviders;

        use crate::balance::{
            AlternativeSet, BalanceConfig, BalancePolicy, BalancedClient, ModelConfig, QueueModel,
        };
        use crate::codec::{CodecId, DecodeError, EncodeError, Wire};
        use crate::protocol::{MethodId, RpcMethod, SchemaVersion};
        use crate::{RpcConfig, RpcDriver};

        struct NotSync(Cell<u8>);
        impl Wire for NotSync {
            const CODEC: CodecId = CodecId::new(0x8123);
            fn encode(&self, buf: &mut Vec<u8>) -> Result<(), EncodeError> {
                buf.push(self.0.get());
                Ok(())
            }
            fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
                Ok(Self(Cell::new(bytes.first().copied().unwrap_or(0))))
            }
        }
        struct Method;
        impl RpcMethod for Method {
            type Request = NotSync;
            type Reply = NotSync;
            const METHOD: MethodId = MethodId::new(1);
            const SCHEMA: SchemaVersion = SchemaVersion::new(1);
            const NAME: &'static str = "not-sync";
        }
        fn assert_send<T: Send>(_: &T) {}

        let (driver, rpc) =
            RpcDriver::client_only(TokioProviders::new(), RpcConfig::default()).expect("valid");
        let model = QueueModel::new(ModelConfig::default()).expect("valid");
        let client = BalancedClient::<TokioProviders, Method>::new(
            &rpc,
            AlternativeSet::empty(SetVersion::new(0)),
            model,
            BalanceConfig::default(),
        )
        .expect("valid");
        let policy = BalancePolicy::default();
        let call = client.call(NotSync(Cell::new(1)), &policy);
        assert_send(&call);
        drop((call, driver));
    }

    #[test]
    fn a_failed_call_never_claims_less_than_its_most_ambiguous_attempt() {
        let endpoint = Endpoint::new(
            SocketAddr::from(([127, 0, 0, 1], 1)),
            Incarnation::from_raw(1),
            EndpointToken::from_parts(1, 1),
        );
        let record = |outcome| AttemptRecord {
            alternative: 0,
            endpoint,
            kind: AttemptKind::First,
            outcome,
        };
        let refused = record(AttemptOutcome::Failed(RpcError::not_admitted(
            ErrorReason::EndpointNotFound,
        )));
        let none = BalanceError::new(BalanceFailure::NoAlternatives, SetVersion::new(1), vec![]);
        assert_eq!(none.execution(), Execution::NotAdmitted);
        let refused_only = BalanceError::new(
            BalanceFailure::StaleAlternatives,
            SetVersion::new(1),
            vec![refused.clone()],
        );
        assert_eq!(refused_only.execution(), Execution::NotAdmitted);
        // A late loser may have run, even though the last attempt was
        // refused.
        let lagging = BalanceError::new(
            BalanceFailure::AllAlternativesFailed,
            SetVersion::new(1),
            vec![record(AttemptOutcome::Lagging), refused.clone()],
        );
        assert_eq!(lagging.execution(), Execution::MaybeExecuted);
        let declined = BalanceError::new(
            BalanceFailure::Declined,
            SetVersion::new(1),
            vec![refused, record(AttemptOutcome::Declined)],
        );
        assert_eq!(declined.execution(), Execution::Executed);
        assert_eq!(declined.attempts().len(), 2);
    }
}
