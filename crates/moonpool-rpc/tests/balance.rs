//! Balanced calls over real TCP: slow and fast servers, a disconnected
//! server, a destroyed endpoint, late losing replies, and receipt counts
//! observed by the servers themselves, which must agree with the call's
//! retry and duplicate permissions. The scenarios that race real time run
//! on both Tokio runtime flavors.

#![cfg(feature = "prost")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonpool_core::{Providers, TimeProvider, TokioProviders};
use moonpool_rpc::balance::{
    Alternative, AlternativeSet, AttemptKind, AttemptOutcome, BalanceConfig, BalanceFailure,
    BalanceHooks, BalancePolicy, BalancedClient, BusyScoreSelector, DuplicatePolicy, Duplicates,
    Feedback, HedgeBudget, Locality, ModelConfig, QueueModel, ReplaceError, Retry, SetVersion,
    Verdict,
};
use moonpool_rpc::security::{
    AccessRequest, Credential, CredentialError, Principal, RequestVerifier, SecurityConfig,
};
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, IncomingRequest, MethodId, RpcConfig, RpcDriver, RpcError,
    RpcHandle, RpcMethod, SchemaVersion, ServiceRef,
};

#[derive(Clone, PartialEq, prost::Message)]
struct Job {
    #[prost(uint64, tag = "1")]
    id: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct Done {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(string, tag = "2")]
    server: String,
    #[prost(bool, tag = "3")]
    behind: bool,
}

struct Work;
impl RpcMethod for Work {
    type Request = Job;
    type Reply = Done;
    const METHOD: MethodId = MethodId::new(0xBA1A);
    const SCHEMA: SchemaVersion = SchemaVersion::new(1);
    const NAME: &'static str = "work";
}

/// How a test server treats each request, after recording its receipt.
#[derive(Clone, Copy)]
enum Mode {
    Fast,
    Slow(Duration),
    /// Hold the reply handle forever.
    Blackhole,
    /// Drop the reply handle: a broken promise, ambiguous for the caller.
    DropReply,
    /// Answer "temporarily behind" (application-defined).
    Behind,
}

struct Server {
    rpc: RpcHandle<TokioProviders>,
    service: ServiceRef<Work>,
    receipts: Arc<Mutex<Vec<u64>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Server {
    async fn start(name: &'static str, mode: Mode) -> Self {
        Self::start_with(name, mode, RpcConfig::default(), AccessClass::Public).await
    }

    async fn start_with(
        name: &'static str,
        mode: Mode,
        config: RpcConfig,
        access: AccessClass,
    ) -> Self {
        let (driver, rpc) = RpcDriver::listen(TokioProviders::new(), "127.0.0.1:0", config)
            .await
            .expect("bind an ephemeral port");
        let driver = tokio::spawn(async move {
            let _ = driver.run().await;
        });
        let (service, mut stream) = rpc.register::<Work>(access).expect("register");
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&receipts);
        let time = TokioProviders::new();
        let handler = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(IncomingRequest { request, reply }) = stream.recv().await {
                seen.lock().expect("receipts").push(request.id);
                let done = Done {
                    id: request.id,
                    server: name.to_string(),
                    behind: matches!(mode, Mode::Behind),
                };
                match mode {
                    Mode::Fast | Mode::Behind => {
                        let _ = reply.send(&done);
                    }
                    Mode::Slow(delay) => {
                        let time = time.clone();
                        tokio::spawn(async move {
                            let _ = time.time().sleep(delay).await;
                            let _ = reply.send(&done);
                        });
                    }
                    Mode::Blackhole => held.push(reply),
                    Mode::DropReply => drop(reply),
                }
            }
        });
        Self {
            rpc,
            service,
            receipts,
            tasks: vec![driver, handler],
        }
    }

    fn receipts(&self) -> Vec<u64> {
        self.receipts.lock().expect("receipts").clone()
    }

    /// Stop serving: the listener, every connection and the endpoint go.
    fn stop(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn client() -> (RpcHandle<TokioProviders>, tokio::task::JoinHandle<()>) {
    let (driver, rpc) =
        RpcDriver::client_only(TokioProviders::new(), RpcConfig::default()).expect("valid");
    (
        rpc,
        tokio::spawn(async move {
            let _ = driver.run().await;
        }),
    )
}

/// The client runs on machine `m1` in `dc1`.
fn config() -> BalanceConfig {
    BalanceConfig {
        locality: Locality::new("m1", "dc1"),
        ..BalanceConfig::default()
    }
}

/// A model with `budget` hedge units ready.
fn model(budget: f64) -> QueueModel {
    QueueModel::new(ModelConfig {
        hedge_budget: HedgeBudget {
            initial: budget,
            growth: 0.0,
            max: budget.max(1.0),
        },
        ..ModelConfig::default()
    })
    .expect("valid model")
}

/// `first` on the caller's machine, then the others farther away: the
/// queue selector tries them in this order while `first` is healthy.
fn set(version: u64, first: &Server, others: &[&Server]) -> AlternativeSet<Work> {
    let mut alternatives = vec![Alternative::new(
        first.service.clone(),
        Locality::new("m1", "dc1"),
    )];
    alternatives.extend(
        others
            .iter()
            .map(|server| Alternative::new(server.service.clone(), Locality::in_datacenter("dc2"))),
    );
    AlternativeSet::new(SetVersion::new(version), alternatives).expect("valid set")
}

/// Classifies `behind` replies as temporarily behind, and optionally
/// compares every request with a second server.
#[derive(Default)]
struct Hooks {
    compare: bool,
    compared: Arc<Mutex<u32>>,
}

impl BalanceHooks<Work> for Hooks {
    fn classify(&self, reply: &Done) -> Verdict {
        if reply.behind {
            Verdict::Behind
        } else {
            Verdict::Accept(Feedback::default())
        }
    }

    fn wants_comparison(&self, _request: &Job) -> bool {
        self.compare
    }

    fn compare(&self, winner: &Done, copy: Option<&Result<Done, RpcError>>) -> Result<(), String> {
        *self.compared.lock().expect("compared") += 1;
        match copy {
            Some(Ok(copy)) if copy.server != winner.server => Err(format!(
                "{} answered for {}, {} for {}",
                winner.server, winner.id, copy.server, copy.id
            )),
            _ => Ok(()),
        }
    }
}

/// Poll `condition` on real time for up to five seconds.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

async fn settled(model: &QueueModel) -> bool {
    eventually(|| {
        let stats = model.stats();
        stats.in_flight == 0 && stats.lagging == 0 && stats.reservations == stats.releases
    })
    .await
}

/// A slow first choice is hedged to a fast second one; the fast reply
/// completes the caller, the slow one arrives later as a late loser that
/// still updates the model. Both servers saw the request exactly once.
async fn hedge_wins_and_the_late_loser_updates_the_model() {
    let slow = Server::start("slow", Mode::Slow(Duration::from_millis(300))).await;
    let fast = Server::start("fast", Mode::Fast).await;
    let (rpc, _driver) = client();
    let model = model(4.0);
    let collector = tokio::spawn(model.collect_lagging());
    let balanced =
        BalancedClient::new(&rpc, set(1, &slow, &[&fast]), model.clone(), config()).expect("valid");

    let done = balanced
        .call(Job { id: 1 }, &BalancePolicy::hedged())
        .await
        .expect("the hedge answers");
    assert_eq!(done.reply.server, "fast");
    assert_eq!(done.alternative, 1);
    let kinds: Vec<_> = done
        .attempts
        .iter()
        .map(|a| (a.kind, a.outcome.clone()))
        .collect();
    assert!(
        kinds.contains(&(AttemptKind::Hedge, AttemptOutcome::Replied)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&(AttemptKind::First, AttemptOutcome::Lagging)),
        "{kinds:?}"
    );
    assert_eq!(slow.receipts(), vec![1]);
    assert_eq!(fast.receipts(), vec![1]);

    // The late loser lands and releases its reservation with a sample.
    assert!(settled(&model).await, "{:?}", model.stats());
    let stats = model.stats();
    assert_eq!(stats.reservations, 2);
    assert_eq!(stats.lagging_completed, 1);
    assert_eq!(stats.copies_granted, 1);
    let latency = model
        .measurement(&slow.service.endpoint(), TokioProviders::new().time().now())
        .latency;
    assert!(latency >= Duration::from_millis(250), "{latency:?}");
    collector.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hedge_wins_and_the_late_loser_updates_the_model_multi_thread() {
    hedge_wins_and_the_late_loser_updates_the_model().await;
}

#[tokio::test(flavor = "current_thread")]
async fn hedge_wins_and_the_late_loser_updates_the_model_current_thread() {
    hedge_wins_and_the_late_loser_updates_the_model().await;
}

/// The same ambiguous first attempt (a broken promise) under each
/// permission: only an explicit retry permission reaches the second
/// server, and at-most-once never sends a copy even with budget to spare.
async fn receipts_follow_the_permissions() {
    let dropping = Server::start("dropping", Mode::DropReply).await;
    let fast = Server::start("fast", Mode::Fast).await;
    let (rpc, _driver) = client();
    let model = model(10.0);
    let collector = tokio::spawn(model.collect_lagging());
    let balanced = BalancedClient::new(&rpc, set(1, &dropping, &[&fast]), model.clone(), config())
        .expect("valid");

    // Default: at most once, no copies. The broken promise ends the call.
    let error = balanced
        .call(Job { id: 1 }, &BalancePolicy::default())
        .await
        .expect_err("ambiguous without retry permission");
    assert!(matches!(
        error.failure(),
        BalanceFailure::Rejected(e) if *e.reason() == ErrorReason::BrokenPromise
    ));
    assert_eq!(error.execution(), Execution::MaybeExecuted);
    assert_eq!(dropping.receipts(), vec![1]);
    assert!(fast.receipts().is_empty());

    // Retry permission only: the second server executes it too.
    let done = balanced
        .call(Job { id: 2 }, &BalancePolicy::idempotent())
        .await
        .expect("retried");
    assert_eq!(done.reply.server, "fast");
    assert!(done.attempts.iter().all(|a| a.kind != AttemptKind::Hedge));
    assert_eq!(dropping.receipts(), vec![1, 2]);
    assert_eq!(fast.receipts(), vec![2]);

    // A retry flag never grants a copy: a slow first choice is waited on.
    let slow = Server::start("slow", Mode::Slow(Duration::from_millis(100))).await;
    let only_slow_first =
        BalancedClient::new(&rpc, set(1, &slow, &[&fast]), model.clone(), config()).expect("valid");
    let policy = BalancePolicy {
        retry: Retry::AfterAmbiguous,
        duplicates: Duplicates::Forbidden,
        ..BalancePolicy::default()
    };
    let done = only_slow_first
        .call(Job { id: 3 }, &policy)
        .await
        .expect("slow answers");
    assert_eq!(done.reply.server, "slow");
    assert_eq!(done.attempts.len(), 1);
    assert_eq!(fast.receipts(), vec![2], "no copy reached the fast server");
    assert_eq!(model.stats().copies_granted, 0);

    // Hedge permission only, the first choice black-holed: the hedge
    // answers, the first attempt stays ambiguous and becomes a late loser.
    let hole = Server::start("hole", Mode::Blackhole).await;
    let hedging =
        BalancedClient::new(&rpc, set(1, &hole, &[&fast]), model.clone(), config()).expect("valid");
    let done = hedging
        .call(Job { id: 4 }, &BalancePolicy::hedged())
        .await
        .expect("hedged");
    assert_eq!(done.reply.server, "fast");
    assert_eq!(hole.receipts(), vec![4]);
    assert_eq!(fast.receipts(), vec![2, 4]);

    // Neither permission, black-holed first choice with an attempt
    // deadline: ambiguous timeout, nothing else sent.
    let policy = BalancePolicy {
        attempt_timeout: Some(Duration::from_millis(200)),
        ..BalancePolicy::default()
    };
    let error = hedging
        .call(Job { id: 5 }, &policy)
        .await
        .expect_err("times out");
    assert!(matches!(
        error.failure(),
        BalanceFailure::Rejected(e) if *e.reason() == ErrorReason::Timeout
    ));
    assert_eq!(error.execution(), Execution::MaybeExecuted);
    assert_eq!(hole.receipts(), vec![4, 5]);
    assert_eq!(fast.receipts(), vec![2, 4]);
    // The black-holed late loser is still outstanding; nothing else is.
    assert!(
        eventually(|| model.stats().in_flight == 1).await,
        "{:?}",
        model.stats()
    );
    drop(hole);
    assert!(settled(&model).await, "{:?}", model.stats());
    collector.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipts_follow_the_permissions_multi_thread() {
    receipts_follow_the_permissions().await;
}

#[tokio::test(flavor = "current_thread")]
async fn receipts_follow_the_permissions_current_thread() {
    receipts_follow_the_permissions().await;
}

/// A disconnected server and a destroyed endpoint are skipped without any
/// permission: neither attempt can have executed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_server_and_destroyed_endpoint_fail_over() {
    let gone = Server::start("gone", Mode::Fast).await;
    let fast = Server::start("fast", Mode::Fast).await;
    gone.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (rpc, _driver) = client();
    let model = model(0.0);
    let balanced =
        BalancedClient::new(&rpc, set(1, &gone, &[&fast]), model.clone(), config()).expect("valid");
    let done = balanced
        .call(Job { id: 1 }, &BalancePolicy::default())
        .await
        .expect("fails over");
    assert_eq!(done.reply.server, "fast");
    assert!(matches!(
        &done.attempts[0].outcome,
        AttemptOutcome::Failed(e) if e.execution() == Execution::NotAdmitted
    ));
    assert!(gone.receipts().is_empty());

    // A live server whose endpoint was destroyed: EndpointNotFound, not
    // admitted, and remembered so the next call skips it.
    let (destroyed, stream) = fast
        .rpc
        .register::<Work>(AccessClass::Public)
        .expect("register");
    drop(stream);
    let set = AlternativeSet::new(
        SetVersion::new(2),
        vec![
            Alternative::new(destroyed, Locality::new("m1", "dc1")),
            Alternative::new(fast.service.clone(), Locality::in_datacenter("dc2")),
        ],
    )
    .expect("valid");
    balanced.replace(set).expect("newer");
    let done = balanced
        .call(Job { id: 2 }, &BalancePolicy::default())
        .await
        .expect("fails over");
    assert_eq!(done.alternative, 1);
    assert!(matches!(
        &done.attempts[0].outcome,
        AttemptOutcome::Failed(e) if *e.reason() == ErrorReason::EndpointNotFound
    ));
    let done = balanced
        .call(Job { id: 3 }, &BalancePolicy::default())
        .await
        .expect("healthy");
    assert_eq!(done.attempts.len(), 1, "the destroyed endpoint is skipped");
    assert_eq!(fast.receipts(), vec![1, 2, 3]);
    assert!(settled(&model).await);
}

/// Every alternative permanently gone: the set is reported stale, never
/// refreshed; only an explicit, newer set brings calls back. An empty set
/// fails without an attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_and_empty_sets_are_reported_and_replaced_explicitly() {
    let fast = Server::start("fast", Mode::Fast).await;
    let (rpc, _driver) = client();
    let model = model(0.0);
    let (destroyed, stream) = fast
        .rpc
        .register::<Work>(AccessClass::Public)
        .expect("register");
    drop(stream);
    let set = AlternativeSet::new(SetVersion::new(1), vec![Alternative::anywhere(destroyed)])
        .expect("valid");
    let balanced = BalancedClient::new(&rpc, set, model.clone(), config()).expect("valid");
    let changed = balanced.on_status_change();
    let error = balanced
        .call(Job { id: 1 }, &BalancePolicy::idempotent())
        .await
        .expect_err("stale");
    assert_eq!(error.failure(), &BalanceFailure::StaleAlternatives);
    assert_eq!(error.execution(), Execution::NotAdmitted);
    assert_eq!(error.version(), SetVersion::new(1));
    changed.await;
    assert!(balanced.status().stale);

    // Older or equal versions are refused; a newer one is installed.
    assert!(matches!(
        balanced.replace(AlternativeSet::empty(SetVersion::new(1))),
        Err(ReplaceError::NotNewer { .. })
    ));
    balanced
        .replace(AlternativeSet::empty(SetVersion::new(2)))
        .expect("newer");
    let error = balanced
        .call(Job { id: 2 }, &BalancePolicy::default())
        .await
        .expect_err("empty");
    assert_eq!(error.failure(), &BalanceFailure::NoAlternatives);
    assert!(error.attempts().is_empty());
    balanced.replace(set_of(3, &fast)).expect("newer");
    assert!(!balanced.status().stale);
    let done = balanced
        .call(Job { id: 3 }, &BalancePolicy::default())
        .await
        .expect("fresh set");
    assert_eq!(done.version, SetVersion::new(3));
    assert_eq!(fast.receipts(), vec![3]);
    // Duplicate references are refused up front.
    assert!(
        AlternativeSet::new(
            SetVersion::new(4),
            vec![
                Alternative::anywhere(fast.service.clone()),
                Alternative::anywhere(fast.service.clone())
            ]
        )
        .is_err()
    );
}

fn set_of(version: u64, server: &Server) -> AlternativeSet<Work> {
    AlternativeSet::new(
        SetVersion::new(version),
        vec![Alternative::anywhere(server.service.clone())],
    )
    .expect("valid")
}

/// A server answering "temporarily behind" (application-defined) is
/// declined without a retry permission, excluded for a while, and the
/// next call goes straight to the healthy one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporarily_behind_is_classified_and_backed_off() {
    let behind = Server::start("behind", Mode::Behind).await;
    let fast = Server::start("fast", Mode::Fast).await;
    let (rpc, _driver) = client();
    let model = model(0.0);
    let balanced = BalancedClient::new(&rpc, set(1, &behind, &[&fast]), model.clone(), config())
        .expect("valid")
        .with_hooks(Hooks::default());
    let done = balanced
        .call(Job { id: 1 }, &BalancePolicy::default())
        .await
        .expect("fails over");
    assert_eq!(done.reply.server, "fast");
    assert_eq!(done.attempts[0].outcome, AttemptOutcome::Declined);
    let now = TokioProviders::new().time().now();
    assert!(
        model
            .measurement(&behind.service.endpoint(), now)
            .excluded_until
            .is_some()
    );
    let done = balanced
        .call(Job { id: 2 }, &BalancePolicy::default())
        .await
        .expect("healthy");
    assert_eq!(done.attempts.len(), 1);
    assert_eq!(behind.receipts(), vec![1]);
    assert_eq!(fast.receipts(), vec![1, 2]);

    // Every alternative behind: declined until the attempts run out.
    let behind_only = BalancedClient::new(&rpc, set_of(1, &behind), model.clone(), config())
        .expect("valid")
        .with_hooks(Hooks::default());
    let policy = BalancePolicy {
        max_attempts: 2,
        ..BalancePolicy::default()
    };
    let error = behind_only
        .call(Job { id: 3 }, &policy)
        .await
        .expect_err("declined");
    assert_eq!(error.failure(), &BalanceFailure::Declined);
    // The handler ran twice, but declined without effect both times.
    assert_eq!(error.execution(), Execution::NotAdmitted);
    assert!(
        error
            .attempts()
            .iter()
            .all(|attempt| attempt.outcome == AttemptOutcome::Declined)
    );
    assert_eq!(behind.receipts(), vec![1, 3, 3]);
    assert!(settled(&model).await);
}

/// When every alternative looks failed, the call still probes each one
/// once after the bounded wait, including one it already tried in this
/// call, before reporting that all alternatives failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_alternatives_are_probed_before_giving_up() {
    let gone = Server::start("gone", Mode::Fast).await;
    gone.stop();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (driver, rpc) = RpcDriver::client_only(
        TokioProviders::new(),
        RpcConfig {
            peer: moonpool_rpc::PeerPolicy {
                // The first refused dial marks the address failed.
                failure_detection_delay: Duration::ZERO,
                ..moonpool_rpc::PeerPolicy::default()
            },
            ..RpcConfig::default()
        },
    )
    .expect("valid");
    let _driver = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    let balanced = BalancedClient::new(
        &rpc,
        set_of(1, &gone),
        model(0.0),
        BalanceConfig {
            all_failed_wait: Duration::from_millis(50),
            ..config()
        },
    )
    .expect("valid");
    let error = balanced
        .call(Job { id: 1 }, &BalancePolicy::default())
        .await
        .expect_err("nothing answers");
    assert_eq!(error.failure(), &BalanceFailure::AllAlternativesFailed);
    // The first attempt, then the probe of the same (already tried)
    // alternative; both refused before admission.
    assert_eq!(error.attempts().len(), 2, "{:?}", error.attempts());
    assert_eq!(error.execution(), Execution::NotAdmitted);
    assert!(balanced.status().all_failed);
}

/// Under the duplicate permission, an accepted comparison copy wins when
/// the primary attempt ends ambiguously: nothing new is sent, and the
/// usable reply is not thrown away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_comparison_copy_wins_when_the_primary_cannot() {
    let dropping = Server::start("dropping", Mode::DropReply).await;
    let fast = Server::start("fast", Mode::Slow(Duration::from_millis(50))).await;
    let (rpc, _driver) = client();
    let model = model(2.0);
    let collector = tokio::spawn(model.collect_lagging());
    let hooks = Hooks {
        compare: true,
        ..Hooks::default()
    };
    let compared = Arc::clone(&hooks.compared);
    let balanced = BalancedClient::new(&rpc, set(1, &dropping, &[&fast]), model.clone(), config())
        .expect("valid")
        .with_hooks(hooks);
    let policy = BalancePolicy {
        duplicates: Duplicates::Permitted(DuplicatePolicy {
            max_copies: 1,
            hedge: None,
            compare_within: Duration::from_secs(2),
        }),
        ..BalancePolicy::default()
    };
    let done = balanced
        .call(Job { id: 1 }, &policy)
        .await
        .expect("the copy answers");
    assert_eq!(done.reply.server, "fast");
    assert_eq!(done.alternative, 1);
    assert_eq!(dropping.receipts(), vec![1]);
    assert_eq!(fast.receipts(), vec![1], "no retry was sent");
    assert_eq!(*compared.lock().expect("compared"), 0, "nothing to compare");
    assert!(settled(&model).await);
    collector.abort();
}

/// Cancelling a call in flight hands its attempt to the late-loser
/// collection: the server still answers and the reservation is released
/// with a latency sample.
#[tokio::test(flavor = "current_thread")]
async fn a_cancelled_call_releases_its_reservation_late() {
    let slow = Server::start("slow", Mode::Slow(Duration::from_millis(200))).await;
    let (rpc, _driver) = client();
    let model = model(0.0);
    let collector = tokio::spawn(model.collect_lagging());
    let balanced =
        BalancedClient::new(&rpc, set_of(1, &slow), model.clone(), config()).expect("valid");
    let time = TokioProviders::new();
    let policy = BalancePolicy::default();
    let call = balanced.call(Job { id: 1 }, &policy);
    let cancelled = time.time().timeout(Duration::from_millis(50), call).await;
    assert!(cancelled.is_err(), "the caller gave up");
    assert_eq!(model.stats().lagging, 1);
    assert!(settled(&model).await, "{:?}", model.stats());
    assert_eq!(model.stats().lagging_completed, 1);
    assert_eq!(slow.receipts(), vec![1]);
    collector.abort();
}

/// The hedge budget is shared and bounded: once spent, slow calls wait
/// for their first choice instead of hedging.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_budget_stops_hedging() {
    let slow = Server::start("slow", Mode::Slow(Duration::from_millis(150))).await;
    let fast = Server::start("fast", Mode::Fast).await;
    let (rpc, _driver) = client();
    let model = model(1.0);
    let collector = tokio::spawn(model.collect_lagging());
    let balanced =
        BalancedClient::new(&rpc, set(1, &slow, &[&fast]), model.clone(), config()).expect("valid");
    let first = balanced
        .call(Job { id: 1 }, &BalancePolicy::hedged())
        .await
        .expect("hedged");
    assert_eq!(first.reply.server, "fast");
    assert!(settled(&model).await);
    // The slow server stays first (it is the local one); the hedge is due
    // at once now that it is measured slow, but the budget is spent.
    let second = balanced
        .call(Job { id: 2 }, &BalancePolicy::hedged())
        .await
        .expect("answered");
    let hedges = second
        .attempts
        .iter()
        .filter(|attempt| attempt.kind == AttemptKind::Hedge)
        .count();
    assert_eq!(hedges, 0);
    assert_eq!(model.stats().copies_granted, 1);
    assert!(model.stats().copies_denied >= 1);
    assert_eq!(fast.receipts().len() + slow.receipts().len(), 3);
    collector.abort();
}

/// Reports the "busy" server's replies with a high busy score.
struct BusyHooks;

impl BalanceHooks<Work> for BusyHooks {
    fn classify(&self, reply: &Done) -> Verdict {
        Verdict::Accept(Feedback {
            penalty: None,
            busy: Some(if reply.server == "busy" { 5000 } else { 0 }),
        })
    }
}

/// Busy-score selection: once scores arrive, the idle server takes nearly
/// every call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_scores_steer_selection() {
    let busy = Server::start("busy", Mode::Fast).await;
    let idle = Server::start("idle", Mode::Fast).await;
    let (rpc, _driver) = client();
    let model = model(0.0);
    let balanced = BalancedClient::new(&rpc, set(1, &busy, &[&idle]), model.clone(), config())
        .expect("valid")
        .with_hooks(BusyHooks)
        .with_selector(BusyScoreSelector::default());
    // Learn both scores first: unknown scores count as idle, so both get
    // picked early on.
    let mut id = 0;
    while (busy.receipts().is_empty() || idle.receipts().is_empty()) && id < 64 {
        balanced
            .call(Job { id }, &BalancePolicy::default())
            .await
            .expect("answered");
        id += 1;
    }
    let (busy_before, idle_before) = (busy.receipts().len(), idle.receipts().len());
    for id in 100..160 {
        balanced
            .call(Job { id }, &BalancePolicy::default())
            .await
            .expect("answered");
    }
    let busy_calls = busy.receipts().len() - busy_before;
    let idle_calls = idle.receipts().len() - idle_before;
    assert_eq!(busy_calls + idle_calls, 60);
    assert!(busy_calls <= 3, "busy {busy_calls}, idle {idle_calls}");
    assert!(settled(&model).await);
}

/// Comparison copies are opt-in, need the duplicate permission and the
/// budget, and a disagreement fails the call as executed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comparison_hooks_cannot_bypass_the_duplicate_permission() {
    let a = Server::start("a", Mode::Fast).await;
    let b = Server::start("b", Mode::Fast).await;
    let (rpc, _driver) = client();
    let model = model(2.0);
    let collector = tokio::spawn(model.collect_lagging());
    let hooks = Hooks {
        compare: true,
        ..Hooks::default()
    };
    let compared = Arc::clone(&hooks.compared);
    let balanced = BalancedClient::new(&rpc, set(1, &a, &[&b]), model.clone(), config())
        .expect("valid")
        .with_hooks(hooks);

    // Without the permission the hook's wish is ignored.
    let done = balanced
        .call(Job { id: 1 }, &BalancePolicy::default())
        .await
        .expect("single attempt");
    assert_eq!(done.attempts.len(), 1);
    assert!(b.receipts().is_empty());
    assert_eq!(model.stats().copies_granted, 0);

    // With it, both servers run the request and the hook refuses the
    // mismatch.
    let policy = BalancePolicy {
        duplicates: Duplicates::Permitted(DuplicatePolicy {
            max_copies: 1,
            hedge: None,
            compare_within: Duration::from_secs(2),
        }),
        ..BalancePolicy::default()
    };
    let error = balanced
        .call(Job { id: 2 }, &policy)
        .await
        .expect_err("replies differ");
    assert!(matches!(error.failure(), BalanceFailure::Comparison(_)));
    assert_eq!(error.execution(), Execution::Executed);
    assert_eq!(a.receipts(), vec![1, 2]);
    assert_eq!(b.receipts(), vec![2]);
    assert_eq!(*compared.lock().expect("compared"), 1);
    assert!(settled(&model).await);
    collector.abort();
}

/// The one credential the verifying servers below accept.
const TOKEN: &str = "balanced-bearer-token";

struct OneToken;
impl RequestVerifier for OneToken {
    fn verify(
        &self,
        _request: &AccessRequest<'_>,
        credential: &[u8],
    ) -> Result<Principal, CredentialError> {
        if credential == TOKEN.as_bytes() {
            Ok(Principal::new("balancer"))
        } else {
            Err(CredentialError::BadSignature)
        }
    }
}

fn verifying() -> RpcConfig {
    RpcConfig {
        security: SecurityConfig::enforced(OneToken).accept_credentials_over_plaintext(),
        ..RpcConfig::default()
    }
}

fn credentialed_client() -> (RpcHandle<TokioProviders>, tokio::task::JoinHandle<()>) {
    let config = RpcConfig::default();
    let (driver, rpc) = RpcDriver::client_only(
        TokioProviders::new(),
        RpcConfig {
            security: config.security.clone().send_credentials_over_plaintext(),
            ..config
        },
    )
    .expect("valid");
    (
        rpc,
        tokio::spawn(async move {
            let _ = driver.run().await;
        }),
    )
}

fn hedged() -> BalancePolicy {
    BalancePolicy {
        duplicates: Duplicates::Permitted(DuplicatePolicy {
            max_copies: 1,
            hedge: Some(moonpool_rpc::balance::HedgeTiming::default()),
            compare_within: Duration::from_secs(1),
        }),
        attempt_timeout: Some(Duration::from_secs(5)),
        ..BalancePolicy::default()
    }
}

/// `BalancedClient::with_credentials` attaches the credential to every
/// attempt, the hedge included: against private endpoints of verifying
/// servers, a slow first choice is hedged and the credentialed copy wins.
/// Without credentials the first refusal ends the call: a denial is the
/// caller's problem, never failed over (no other server sees it).
async fn credentials_reach_every_balanced_attempt() {
    let slow = Server::start_with(
        "slow",
        Mode::Slow(Duration::from_millis(400)),
        verifying(),
        AccessClass::Private,
    )
    .await;
    let fast = Server::start_with("fast", Mode::Fast, verifying(), AccessClass::Private).await;
    let (rpc, driver) = credentialed_client();
    let model = model(10.0);
    let credentialed = BalancedClient::new(&rpc, set(1, &slow, &[&fast]), model.clone(), config())
        .expect("valid")
        .with_credentials(Credential::bearer(TOKEN));
    let collector = tokio::spawn({
        let model = model.clone();
        async move { model.collect_lagging().await }
    });
    let done = credentialed
        .call(Job { id: 1 }, &hedged())
        .await
        .expect("a credentialed hedged call succeeds");
    assert_eq!(done.reply.server, "fast");
    assert!(
        done.attempts
            .iter()
            .any(|attempt| attempt.kind == AttemptKind::Hedge),
        "{:?}",
        done.attempts
    );
    assert!(
        eventually(|| slow.receipts() == [1]).await,
        "the slow copy ran too"
    );
    assert_eq!(fast.receipts(), [1]);
    // A replaced set keeps the credential.
    credentialed
        .replace(set(2, &fast, &[&slow]))
        .expect("newer set");
    let again = credentialed
        .call(Job { id: 2 }, &BalancePolicy::default())
        .await
        .expect("the replaced set carries the credential");
    assert_eq!(again.reply.id, 2);

    // Anonymous: refused by the first alternative, no failover.
    let anonymous =
        BalancedClient::new(&rpc, set(1, &fast, &[&slow]), model.clone(), config()).expect("valid");
    let error = anonymous
        .call(Job { id: 3 }, &hedged())
        .await
        .expect_err("private endpoints refuse anonymous calls");
    assert!(
        matches!(
            error.failure(),
            BalanceFailure::Rejected(rejected)
                if matches!(rejected.reason(), ErrorReason::Unauthenticated(CredentialError::Missing))
        ),
        "{error:?}"
    );
    assert_eq!(error.execution(), Execution::NotAdmitted);
    assert_eq!(error.attempts().len(), 1, "{:?}", error.attempts());
    assert!(!fast.receipts().contains(&3) && !slow.receipts().contains(&3));
    assert!(
        !anonymous.status().stale,
        "a denial never marks an alternative dead"
    );
    assert!(settled(&model).await);
    collector.abort();
    driver.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_reach_every_balanced_attempt_multi_thread() {
    credentials_reach_every_balanced_attempt().await;
}

#[tokio::test(flavor = "current_thread")]
async fn credentials_reach_every_balanced_attempt_current_thread() {
    credentials_reach_every_balanced_attempt().await;
}

/// A server draining for a graceful shutdown refuses the attempt
/// (`ServerShuttingDown`, never admitted): the call fails over to another
/// alternative, and the draining one is excluded like an overloaded one.
#[tokio::test]
async fn a_draining_server_is_failed_over_and_excluded() {
    let draining = Server::start("draining", Mode::Blackhole).await;
    let healthy = Server::start("healthy", Mode::Fast).await;
    let (rpc, driver) = client();
    // Something owed keeps the drain going; the call also opens the
    // session the balancer reuses (the listener closes on shutdown).
    let owed = {
        let client = draining.service.bind(&rpc);
        tokio::spawn(async move { client.try_get_reply(&Job { id: 0 }).await })
    };
    assert!(eventually(|| draining.receipts() == [0]).await);
    let shutdown = {
        let rpc = draining.rpc.clone();
        tokio::spawn(async move { rpc.shutdown(Duration::from_secs(30)).await })
    };
    assert!(eventually(|| draining.rpc.is_shutting_down()).await);
    let model = model(0.0);
    let balanced = BalancedClient::new(
        &rpc,
        set(1, &draining, &[&healthy]),
        model.clone(),
        config(),
    )
    .expect("valid");
    let done = balanced
        .call(Job { id: 1 }, &BalancePolicy::default())
        .await
        .expect("fails over to the healthy server");
    assert_eq!(done.reply.server, "healthy");
    assert!(
        done.attempts.iter().any(|attempt| matches!(
            &attempt.outcome,
            AttemptOutcome::Failed(error) if *error.reason() == ErrorReason::ServerShuttingDown
        )),
        "{:?}",
        done.attempts
    );
    assert!(model.stats().exclusions >= 1, "{:?}", model.stats());
    assert!(!draining.receipts().contains(&1));
    owed.abort();
    shutdown.abort();
    driver.abort();
}
