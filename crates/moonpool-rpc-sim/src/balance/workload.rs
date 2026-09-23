//! The surviving client: balances jobs over the servers it learned from
//! the directory, under every combination of retry and duplicate
//! permission, cancels some calls mid-flight, replaces its set explicitly,
//! and judges everything against the receipt ledger and its own attempt
//! ledger.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::balance::{
    Alternative, AlternativeSet, AttemptEnd, AttemptEvent, AttemptKind, AttemptOutcome,
    BalanceConfig, BalanceError, BalanceFailure, BalanceHooks, BalancePolicy, Balanced,
    BalancedClient, DuplicatePolicy, Duplicates, Feedback, HedgeBudget, HedgeTiming, Locality,
    ModelConfig, QueueModel, Retry, SetVersion, Verdict,
};
use moonpool_rpc::{ErrorReason, Execution, RpcDriver, RpcError, ServiceRef};
use moonpool_sim::{
    RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult, TimeProvider,
    Workload, assert_always, assert_sometimes,
};

use super::messages::{Done, Job, Work};
use super::state::{Ledger, SCRIPT_DONE_KEY, WORKLOAD_IP_KEY, WORKLOAD_LABEL};
use super::{BalanceRecord, BalanceRecords};
use crate::foundations::state::Board;
use crate::foundations::{report_stats, rpc_config};

/// One operation of the workload's alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BalanceOp {
    /// Default permissions: at most once, no copies.
    AtMostOnce,
    /// Retry after an ambiguous attempt; no copies.
    Idempotent,
    /// Hedging; no retry after an ambiguous attempt.
    Hedged,
    /// A comparison copy (no hedge), judged by the comparison hook.
    Compare,
    /// A hedged call the caller abandons during its first attempt or
    /// during its hedge.
    Cancel,
    /// Learn the directory and install a newer set.
    Refresh,
}

impl BalanceOp {
    const ALL: [Self; 6] = [
        Self::AtMostOnce,
        Self::Idempotent,
        Self::Hedged,
        Self::Compare,
        Self::Cancel,
        Self::Refresh,
    ];

    /// The permissions a call of this kind runs under.
    fn policy(self) -> BalancePolicy {
        let timeout = Some(Duration::from_millis(1500));
        match self {
            Self::AtMostOnce | Self::Refresh => BalancePolicy {
                attempt_timeout: timeout,
                ..BalancePolicy::default()
            },
            Self::Idempotent => BalancePolicy {
                retry: Retry::AfterAmbiguous,
                attempt_timeout: timeout,
                ..BalancePolicy::default()
            },
            Self::Hedged | Self::Cancel => BalancePolicy {
                duplicates: Duplicates::Permitted(DuplicatePolicy {
                    max_copies: 1,
                    hedge: Some(HedgeTiming::default()),
                    compare_within: Duration::from_secs(1),
                }),
                attempt_timeout: Some(Duration::from_secs(2)),
                ..BalancePolicy::default()
            },
            Self::Compare => BalancePolicy {
                duplicates: Duplicates::Permitted(DuplicatePolicy {
                    max_copies: 1,
                    hedge: None,
                    compare_within: Duration::from_millis(800),
                }),
                attempt_timeout: timeout,
                ..BalancePolicy::default()
            },
        }
    }
}

/// How many handler runs (declined ones aside) a policy permits: one per
/// sequential attempt it may repeat after an ambiguous one, plus its
/// concurrent copies.
fn permitted_executions(policy: &BalancePolicy) -> usize {
    let sequential = match policy.retry {
        Retry::AtMostOnce => 1,
        Retry::AfterAmbiguous => policy.max_attempts.max(1),
    };
    let copies = policy
        .duplicates
        .policy()
        .map_or(0, |copies| copies.max_copies);
    usize::try_from(sequential + copies).unwrap_or(usize::MAX)
}

/// The workload's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceCampaignConfig {
    /// Operations per run.
    pub operations: usize,
    /// Relative weight per [`BalanceOp`], in declaration order.
    pub weights: [u32; 6],
    /// Pause between operations, in milliseconds (half-open range).
    pub gap_ms: (u64, u64),
}

impl BalanceCampaignConfig {
    /// The full campaign mix.
    #[must_use]
    pub fn campaign() -> Self {
        Self {
            operations: 70,
            weights: [14, 14, 16, 10, 10, 6],
            gap_ms: (10, 200),
        }
    }
}

/// The balancer's queue model for the campaign: a small hedge budget so
/// it runs out, a small late-loser bound so it is reached, short
/// exclusions.
fn model_config() -> ModelConfig {
    ModelConfig {
        hedge_budget: HedgeBudget {
            initial: 2.0,
            growth: 0.1,
            max: 3.0,
        },
        max_lagging: 8,
        // Shorter than the attempt deadlines, so a held late loser is
        // dropped at its bound rather than timed out by its own attempt.
        lagging_timeout: Duration::from_secs(1),
        exclusion: moonpool_rpc::balance::Backoff {
            initial: Duration::from_millis(300),
            max: Duration::from_secs(2),
            growth: 2.0,
            jitter: 0.25,
        },
        ..ModelConfig::default()
    }
}

/// The attempt ledger, fed by the balancer's observation hook: every
/// attempt started and ended, independently of the queue model.
#[derive(Default)]
struct Observer {
    started: AtomicU64,
    ended: AtomicU64,
    ended_late: AtomicU64,
    dropped: AtomicU64,
    hedges: AtomicU64,
}

impl Observer {
    fn load(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// The campaign's hooks: "behind" replies are declined, the server's
/// penalty is fed back, jobs flagged for comparison get a copy whose value
/// must match the winner's.
struct Hooks {
    observer: Arc<Observer>,
}

impl BalanceHooks<Work> for Hooks {
    fn classify(&self, reply: &Done) -> Verdict {
        if reply.behind {
            Verdict::Behind
        } else {
            Verdict::Accept(Feedback {
                penalty: Some(reply.penalty),
                busy: None,
            })
        }
    }

    fn wants_comparison(&self, request: &Job) -> bool {
        request.compare
    }

    fn compare(&self, winner: &Done, copy: Option<&Result<Done, RpcError>>) -> Result<(), String> {
        match copy {
            Some(Ok(copy)) if !copy.behind && copy.value != winner.value => Err(format!(
                "job {}: {} computed {}, {} computed {}",
                winner.id, winner.server, winner.value, copy.server, copy.value
            )),
            _ => Ok(()),
        }
    }

    fn observe(&self, event: &AttemptEvent) {
        match event {
            AttemptEvent::Started { kind, .. } => {
                self.observer.started.fetch_add(1, Ordering::Relaxed);
                if *kind == AttemptKind::Hedge {
                    self.observer.hedges.fetch_add(1, Ordering::Relaxed);
                }
            }
            AttemptEvent::Ended { end, late, .. } => {
                self.observer.ended.fetch_add(1, Ordering::Relaxed);
                if *late {
                    self.observer.ended_late.fetch_add(1, Ordering::Relaxed);
                }
                if *end == AttemptEnd::Dropped {
                    self.observer.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

type Outcome = Option<Result<Balanced<Done>, BalanceError>>;

/// The campaign's workload.
pub struct BalanceWorkload {
    config: BalanceCampaignConfig,
    records: BalanceRecords,
    history: Vec<String>,
    next_id: u64,
    version: u64,
    observer: Arc<Observer>,
    /// A call failed while every alternative was down, and none has
    /// succeeded since.
    failed_during_outage: bool,
    calls: u64,
    recovered: bool,
}

impl BalanceWorkload {
    /// A fresh workload appending its run record to `records`.
    #[must_use]
    pub fn new(config: BalanceCampaignConfig, records: BalanceRecords) -> Self {
        Self {
            config,
            records,
            history: Vec::new(),
            next_id: 0,
            version: 0,
            observer: Arc::new(Observer::default()),
            failed_during_outage: false,
            calls: 0,
            recovered: false,
        }
    }

    fn pick(&self, ctx: &SimContext) -> BalanceOp {
        let total: u32 = self.config.weights.iter().sum();
        let mut draw = ctx.random().random_range(0..total.max(1));
        for (op, weight) in BalanceOp::ALL.into_iter().zip(self.config.weights) {
            if draw < weight {
                return op;
            }
            draw -= weight;
        }
        BalanceOp::AtMostOnce
    }

    /// Learn every server's current reference and install them as a
    /// newer set: the only way the client ever learns a new incarnation.
    fn refresh(&mut self, ctx: &SimContext, balanced: &BalancedClient<SimProviders, Work>) {
        let directory = Ledger::of(ctx.state()).directory();
        let mut alternatives = Vec::new();
        for (server, publication) in &directory {
            let Ok(target) = ServiceRef::<Work>::from_bytes(&publication.reference) else {
                continue;
            };
            let datacenter = server
                .rsplit('.')
                .next()
                .and_then(|octet| octet.parse::<u8>().ok())
                .map_or(0, |octet| octet % 2);
            alternatives.push(Alternative::new(
                target,
                Locality::new(server.clone(), format!("dc{datacenter}")),
            ));
        }
        self.version += 1;
        let count = alternatives.len();
        match AlternativeSet::new(SetVersion::new(self.version), alternatives) {
            Ok(set) => {
                let replaced = balanced.replace(set);
                assert_always!(
                    replaced.is_ok(),
                    "a newer alternative set is always installed"
                );
                self.history
                    .push(format!("refresh v{} ({count} alternatives)", self.version));
            }
            Err(error) => {
                assert_always!(false, "published references form a valid set", {
                    "error" => error.to_string()
                });
            }
        }
    }

    async fn call(
        &mut self,
        ctx: &SimContext,
        balanced: &BalancedClient<SimProviders, Work>,
        op: BalanceOp,
    ) -> bool {
        self.next_id += 1;
        self.calls += 1;
        let id = self.next_id;
        let job = Job {
            id,
            compare: op == BalanceOp::Compare,
        };
        let policy = op.policy();
        let ledger = Ledger::of(ctx.state());
        let outage_at_start = ledger.outage();
        let hedges_before = Observer::load(&self.observer.hedges);
        let outcome: Outcome = if op == BalanceOp::Cancel {
            let wait_for_hedge = ctx.random().random_bool(0.5);
            let give_up = ctx.random().random_range(1..300u64);
            let observer = Arc::clone(&self.observer);
            let cancel = async {
                if wait_for_hedge {
                    for _ in 0..1000 {
                        if Observer::load(&observer.hedges) > hedges_before {
                            break;
                        }
                        let _ = ctx.time().sleep(Duration::from_millis(1)).await;
                    }
                }
                let _ = ctx.time().sleep(Duration::from_millis(give_up)).await;
            };
            moonpool_sim::select! {
                outcome = balanced.call(job, &policy) => Some(outcome),
                () = cancel => None,
            }
        } else {
            Some(balanced.call(job, &policy).await)
        };
        let succeeded = matches!(outcome, Some(Ok(_)));
        let call = Judged {
            op,
            id,
            policy,
            outcome,
            outage: outage_at_start || ledger.outage(),
            hedged: Observer::load(&self.observer.hedges) > hedges_before,
        };
        self.judge(ctx, balanced, &call);
        assert_always!(
            balanced.model().stats().lagging <= model_config().max_lagging,
            "late losers stay within the model's bound"
        );
        succeeded
    }

    /// Judge one call against the receipt ledger.
    fn judge(
        &mut self,
        ctx: &SimContext,
        balanced: &BalancedClient<SimProviders, Work>,
        call: &Judged,
    ) {
        let receipts = Ledger::of(ctx.state()).receipts(call.id);
        let executed = receipts.iter().filter(|receipt| !receipt.declined).count();
        let permitted = permitted_executions(&call.policy);
        assert_always!(
            executed <= permitted,
            "a balanced job never runs more often than its permissions allow",
            { "id" => call.id, "op" => format!("{:?}", call.op), "executed" => executed, "permitted" => permitted }
        );
        let summary = match &call.outcome {
            None => {
                if call.hedged {
                    assert_sometimes!(true, "rpc balance call cancelled during its hedge");
                } else {
                    assert_sometimes!(true, "rpc balance call cancelled during its first attempt");
                }
                "cancelled".to_string()
            }
            Some(Ok(done)) => self.judge_reply(call, done, &receipts),
            Some(Err(error)) => self.judge_error(ctx, balanced, call, error, &receipts),
        };
        self.history
            .push(format!("{} {:?} {summary}", call.id, call.op));
    }

    fn judge_reply(
        &mut self,
        call: &Judged,
        done: &Balanced<Done>,
        receipts: &[super::state::Receipt],
    ) -> String {
        let reply = &done.reply;
        assert_always!(reply.id == call.id, "a balanced reply answers its own job");
        assert_always!(
            receipts.iter().any(|receipt| !receipt.declined
                && receipt.server == reply.server
                && receipt.boot == reply.boot
                && receipt.value == reply.value),
            "a balanced reply comes from a server that ran the job"
        );
        let won = |kind: AttemptKind| {
            done.attempts.iter().any(|attempt| {
                attempt.kind == kind
                    && attempt.alternative == done.alternative
                    && attempt.outcome == AttemptOutcome::Replied
            })
        };
        assert_always!(
            won(AttemptKind::First) || won(AttemptKind::Retry) || won(AttemptKind::Hedge),
            "the winning attempt is a primary attempt that replied"
        );
        assert_sometimes!(won(AttemptKind::Hedge), "rpc balance hedge won the race");
        let any = |check: &dyn Fn(&AttemptOutcome) -> bool| {
            done.attempts.iter().any(|attempt| check(&attempt.outcome))
        };
        assert_sometimes!(
            any(&|outcome| *outcome == AttemptOutcome::Lagging),
            "rpc balance loser still in flight when the winner answered"
        );
        assert_sometimes!(
            any(
                &|outcome| matches!(outcome, AttemptOutcome::Failed(error) if *error.reason() == ErrorReason::EndpointNotFound)
            ),
            "rpc balance destroyed endpoint skipped for a healthy one"
        );
        assert_sometimes!(
            any(&|outcome| *outcome == AttemptOutcome::Declined),
            "rpc balance temporarily-behind reply failed over"
        );
        if call.op == BalanceOp::Idempotent {
            assert_sometimes!(
                any(
                    &|outcome| matches!(outcome, AttemptOutcome::Failed(error) if error.execution() == Execution::MaybeExecuted)
                ),
                "rpc balance ambiguous attempt retried with permission"
            );
        }
        if call.op == BalanceOp::Compare {
            assert_sometimes!(
                done.attempts
                    .iter()
                    .any(|attempt| attempt.kind == AttemptKind::Comparison
                        && attempt.outcome == AttemptOutcome::Replied),
                "rpc balance comparison copy agreed with the winner"
            );
        }
        if self.failed_during_outage && !call.outage {
            assert_sometimes!(
                true,
                "rpc balance call succeeded after every alternative was restored"
            );
            self.failed_during_outage = false;
        }
        format!("ok {}#{} v{}", reply.server, reply.boot, done.version.get())
    }

    fn judge_error(
        &mut self,
        ctx: &SimContext,
        balanced: &BalancedClient<SimProviders, Work>,
        call: &Judged,
        error: &BalanceError,
        receipts: &[super::state::Receipt],
    ) -> String {
        if error.execution() == Execution::NotAdmitted {
            assert_always!(
                receipts.is_empty(),
                "a balanced call reported not admitted never reached a handler"
            );
        }
        match error.failure() {
            BalanceFailure::Comparison(_) => {
                assert_always!(
                    error.execution() == Execution::Executed,
                    "a refused comparison reports the winner executed"
                );
                assert_sometimes!(true, "rpc balance comparison hook refused a divergent copy");
            }
            BalanceFailure::StaleAlternatives => {
                assert_sometimes!(true, "rpc balance stale set reported");
                self.refresh(ctx, balanced);
                assert_always!(
                    !balanced.status().stale,
                    "an explicit replacement clears the stale verdict"
                );
            }
            BalanceFailure::AllAlternativesFailed => {
                assert_sometimes!(true, "rpc balance all alternatives failed");
            }
            BalanceFailure::Rejected(rejected)
                if call.policy.retry == Retry::AtMostOnce
                    && rejected.execution() == Execution::MaybeExecuted =>
            {
                assert_sometimes!(
                    true,
                    "rpc balance ambiguous attempt ended an at-most-once call"
                );
            }
            _ => {}
        }
        if call.outage {
            self.failed_during_outage = true;
            assert_sometimes!(
                true,
                "rpc balance call failed while every alternative was down"
            );
        }
        let reason = match error.failure() {
            BalanceFailure::Rejected(rejected) => format!("rejected {:?}", rejected.reason()),
            other => format!("{other:?}"),
        };
        format!("err {reason} {:?}", error.execution())
    }
}

/// One finished call, as judged.
struct Judged {
    op: BalanceOp,
    id: u64,
    policy: BalancePolicy,
    outcome: Outcome,
    outage: bool,
    hedged: bool,
}

#[async_trait]
impl Workload for BalanceWorkload {
    fn name(&self) -> &'static str {
        "rpc_balance_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.state()
            .publish(WORKLOAD_IP_KEY, ctx.my_ip().to_string());
        let (driver, rpc) = RpcDriver::client_only(ctx.providers().clone(), rpc_config())
            .map_err(|error| SimulationError::InvalidState(format!("rpc config: {error}")))?;
        let model = QueueModel::new(model_config())
            .map_err(|error| SimulationError::InvalidState(format!("model: {error}")))?;
        let balanced = BalancedClient::new(
            &rpc,
            AlternativeSet::empty(SetVersion::new(0)),
            model.clone(),
            BalanceConfig {
                locality: Locality::in_datacenter("dc0"),
                ..BalanceConfig::default()
            },
        )
        .map_err(|error| SimulationError::InvalidState(format!("balance: {error}")))?
        .with_hooks(Hooks {
            observer: Arc::clone(&self.observer),
        });
        let board = Board::of(ctx.state());
        let probe = rpc.probe();
        if let Some(probe) = &probe {
            board.register_probe(WORKLOAD_LABEL, probe.clone());
        }
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = self.drive(ctx, &balanced) => Ok(()),
            () = model.collect_lagging() => Ok(()),
            () = report_stats(&rpc, &board, WORKLOAD_LABEL, ctx) => Ok(()),
        };
        drop(balanced);
        if let Some(probe) = &probe {
            assert_always!(
                probe.is_released(),
                "the balance workload runtime released everything at the end"
            );
        }
        result
    }

    async fn check(&mut self, _ctx: &SimContext) -> SimulationResult<()> {
        self.records
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(BalanceRecord {
                history: std::mem::take(&mut self.history),
                calls: self.calls,
                recovered: self.recovered,
            });
        Ok(())
    }
}

impl BalanceWorkload {
    async fn drive(&mut self, ctx: &SimContext, balanced: &BalancedClient<SimProviders, Work>) {
        let ledger = Ledger::of(ctx.state());
        // Learn the first publications explicitly.
        for _ in 0..40 {
            if ledger.directory().len() == ctx.topology().ips_in_group("balance-server").len() {
                break;
            }
            let _ = ctx.time().sleep(Duration::from_millis(50)).await;
        }
        self.refresh(ctx, balanced);
        for _ in 0..self.config.operations {
            let gap = ctx
                .random()
                .random_range(self.config.gap_ms.0..self.config.gap_ms.1);
            let _ = ctx.time().sleep(Duration::from_millis(gap)).await;
            match self.pick(ctx) {
                BalanceOp::Refresh => self.refresh(ctx, balanced),
                op => {
                    let _ = self.call(ctx, balanced, op).await;
                }
            }
        }
        // After the faults: learn the final set; calls succeed again.
        for _ in 0..1200 {
            if ctx.state().get::<bool>(SCRIPT_DONE_KEY).unwrap_or(false) {
                break;
            }
            let _ = ctx.time().sleep(Duration::from_millis(50)).await;
        }
        let _ = ctx.time().sleep(Duration::from_secs(2)).await;
        let mut recovered = false;
        for _ in 0..6 {
            self.refresh(ctx, balanced);
            if self.call(ctx, balanced, BalanceOp::Idempotent).await {
                recovered = true;
                break;
            }
            let _ = ctx.time().sleep(Duration::from_millis(500)).await;
        }
        self.recovered = recovered;
        // Fallback after recovery: no belief the balancer or the failure
        // monitor formed during the outages keeps it from a healthy set.
        assert_always!(
            recovered,
            "a balanced call succeeds once the faults are over"
        );
        self.quiesce(ctx, balanced.model()).await;
    }

    /// Every late loser ends within its bounds; then the attempt ledger and
    /// the model agree: each attempt started once, ended once, reserved
    /// once and released once.
    async fn quiesce(&self, ctx: &SimContext, model: &QueueModel) {
        for _ in 0..200 {
            let stats = model.stats();
            if stats.in_flight == 0 && stats.lagging == 0 {
                break;
            }
            let _ = ctx.time().sleep(Duration::from_millis(50)).await;
        }
        let stats = model.stats();
        let started = Observer::load(&self.observer.started);
        let ended = Observer::load(&self.observer.ended);
        assert_always!(
            stats.in_flight == 0 && stats.lagging == 0,
            "every late loser ended within its bounds",
            { "in_flight" => stats.in_flight, "lagging" => stats.lagging }
        );
        assert_always!(
            started == ended,
            "every balanced attempt started ended exactly once",
            { "started" => started, "ended" => ended }
        );
        assert_always!(
            stats.reservations == started,
            "every balanced attempt reserved the model exactly once",
            { "reservations" => stats.reservations, "started" => started }
        );
        assert_always!(
            stats.releases == stats.reservations,
            "the queue model released every reservation exactly once",
            { "releases" => stats.releases, "reservations" => stats.reservations }
        );
        assert_sometimes!(
            stats.lagging_completed > 0 && Observer::load(&self.observer.ended_late) > 0,
            "rpc balance late loser updated the model after its call"
        );
        assert_sometimes!(
            stats.copies_denied > 0,
            "rpc balance hedge budget exhausted"
        );
        assert_sometimes!(stats.exclusions > 0, "rpc balance exclusion applied");
        assert_sometimes!(
            Observer::load(&self.observer.dropped) > 0,
            "rpc balance late loser dropped at its bound"
        );
    }
}
