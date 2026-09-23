//! The surviving client: bootstraps through a scripted name, drives every
//! delivery mode against the server, watches the failure monitor, and
//! judges its own outcomes against the execution ledger.

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::{
    AccessClass, AddressState, BootstrapAddress, BootstrapClient, EndpointState, ErrorReason,
    Execution, FailureMonitor, RetryPolicy, RpcDriver, RpcError, RpcHandle, ServiceRef,
    WellKnownRef,
};
use moonpool_sim::{
    RandomProvider, ScriptedResolver, SimContext, SimProviders, SimulationError, SimulationResult,
    TimeProvider, Workload, assert_always, assert_reachable, assert_sometimes,
};

use super::messages::{DIRECTORY_ID, Directory, Done, Execute, Job, Listing, Lookup, Mode};
use super::policy::delivery_config;
use super::state::{
    RESOLVER_KEY, SCRIPT_DONE_KEY, SERVER_NAME, WORKLOAD_IP_KEY, WORKLOAD_LABEL, WORKLOAD_RPC_KEY,
};
use super::{DeliveryRecord, DeliveryRecords, RPC_PORT, report_stats};
use crate::foundations::state::{Board, Ledger};

/// One operation of the workload's alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryOp {
    /// One at-most-once attempt, answered at once.
    Single,
    /// One attempt against a slow handler with a short deadline.
    SingleTimeout,
    /// Reliable delivery, bounded by sustained failure.
    Reliable,
    /// Fire and forget.
    OneWay,
    /// The handler finishes with an explicit no-reply.
    NoReply,
    /// The handler drops its reply handle.
    Broken,
    /// The caller drops the call future mid-flight.
    CallerDrop,
    /// The caller moves on, keeps the attempt, and collects it later.
    Late,
    /// The reply is lost after execution; reliable delivery.
    LostReliable,
    /// The reply is lost after execution; a single attempt.
    LostSingle,
    /// The server crashes after execution; reliable delivery.
    Crash,
    /// A reference from an earlier server incarnation.
    Stale,
    /// Look the server up again through the well-known directory.
    Rediscover,
    /// Sit idle past the idle timeout, then call again.
    Pause,
    /// The server replies and crashes at about the same time; a single
    /// attempt.
    Race,
}

impl DeliveryOp {
    const ALL: [Self; 15] = [
        Self::Single,
        Self::SingleTimeout,
        Self::Reliable,
        Self::OneWay,
        Self::NoReply,
        Self::Broken,
        Self::CallerDrop,
        Self::Late,
        Self::LostReliable,
        Self::LostSingle,
        Self::Crash,
        Self::Stale,
        Self::Rediscover,
        Self::Pause,
        Self::Race,
    ];

    /// Whether the operation's calls are at most once (else reliable).
    fn single_attempt(self) -> bool {
        !matches!(self, Self::Reliable | Self::LostReliable | Self::Crash)
    }
}

/// The workload's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryConfig {
    /// Operations per run.
    pub operations: usize,
    /// Relative weight per [`DeliveryOp`], in declaration order.
    pub weights: [u32; 15],
    /// Pause between operations, in milliseconds (half-open range).
    pub gap_ms: (u64, u64),
}

impl DeliveryConfig {
    /// The full campaign mix.
    #[must_use]
    pub fn campaign() -> Self {
        Self {
            operations: 45,
            weights: [18, 8, 15, 7, 5, 5, 6, 6, 7, 6, 4, 5, 3, 2, 4],
            gap_ms: (10, 200),
        }
    }
}

/// What an outcome proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Replied,
    NotAdmitted,
    Maybe,
    Executed,
}

fn classify<T>(outcome: &Result<T, RpcError>) -> Class {
    match outcome {
        Ok(_) => Class::Replied,
        Err(error) => match error.execution() {
            Execution::NotAdmitted => Class::NotAdmitted,
            Execution::Executed => Class::Executed,
            _ => Class::Maybe,
        },
    }
}

fn describe<T>(outcome: &Result<T, RpcError>) -> String {
    match outcome {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("{:?}/{:?}", error.reason(), error.execution()),
    }
}

/// A client of the server's work endpoint.
type Client = moonpool_rpc::ServiceClient<SimProviders, Execute>;

/// Deadline for calls that should normally complete.
const LONG: Duration = Duration::from_secs(30);

/// One judged call.
struct Call {
    id: u64,
    op: DeliveryOp,
    class: Class,
    /// The error was a disconnect after transmission.
    lost_in_disconnect: bool,
}

/// The campaign's workload.
pub struct DeliveryWorkload {
    config: DeliveryConfig,
    /// This run's runtime, once started.
    rpc: Option<RpcHandle<SimProviders>>,
    /// The idle timeout this run's runtime drew.
    idle: Duration,
    records: DeliveryRecords,
    history: Vec<String>,
    calls: Vec<Call>,
    next_id: u64,
    /// Every listing learned, oldest first.
    listings: Vec<Listing>,
    /// Dynamic references the server declared gone.
    dead: Vec<Vec<u8>>,
}

impl DeliveryWorkload {
    /// A fresh workload appending its run record to `records`.
    #[must_use]
    pub fn new(config: DeliveryConfig, records: DeliveryRecords) -> Self {
        Self {
            config,
            rpc: None,
            idle: Duration::ZERO,
            records,
            history: Vec::new(),
            calls: Vec::new(),
            next_id: 0,
            listings: Vec::new(),
            dead: Vec::new(),
        }
    }

    fn record<T>(&mut self, id: u64, op: DeliveryOp, outcome: &Result<T, RpcError>) {
        let lost_in_disconnect = matches!(
            outcome,
            Err(error) if *error.reason() == ErrorReason::Disconnected
                && error.execution() == Execution::MaybeExecuted
        );
        self.calls.push(Call {
            id,
            op,
            class: classify(outcome),
            lost_in_disconnect,
        });
        self.history
            .push(format!("{id} {op:?} {}", describe(outcome)));
    }

    fn pick(&self, ctx: &SimContext) -> DeliveryOp {
        let total: u32 = self.config.weights.iter().sum();
        let mut draw = ctx.random().random_range(0..total.max(1));
        for (op, weight) in DeliveryOp::ALL.into_iter().zip(self.config.weights) {
            if draw < weight {
                return op;
            }
            draw -= weight;
        }
        DeliveryOp::Single
    }

    fn job(&mut self, mode: Mode, delay_ms: u32) -> Job {
        self.next_id += 1;
        Job {
            id: self.next_id,
            mode: mode as u32,
            delay_ms,
        }
    }

    /// Learn the current listing through the well-known directory, by name,
    /// retrying unambiguous failures.
    async fn discover(
        &mut self,
        bootstrap: &BootstrapClient<SimProviders, ScriptedResolver>,
    ) -> Option<ServiceRef<Execute>> {
        let directory = WellKnownRef::<Directory>::new(
            BootstrapAddress::Host(format!("{SERVER_NAME}:{RPC_PORT}")),
            DIRECTORY_ID,
            AccessClass::Public,
        );
        // A directory read is idempotent: ambiguous attempts may be retried.
        let policy = RetryPolicy {
            attempt_timeout: Duration::from_secs(2),
            max_attempts: Some(60),
            retry_ambiguous: true,
            ..RetryPolicy::default()
        };
        let outcome = bootstrap
            .retry_get_reply(&directory, &Lookup {}, &policy)
            .await;
        self.history
            .push(format!("discover {}", describe(&outcome)));
        let listing = outcome.ok()?;
        if let Some(previous) = self.listings.last()
            && listing.boot > previous.boot
        {
            assert_sometimes!(true, "rpc well-known endpoint answered a new incarnation");
        }
        let stats = bootstrap.stats();
        if stats.invalidations > 0 {
            assert_sometimes!(true, "rpc bootstrap re-resolved after a connection failure");
        }
        if stats.lookup_failures > 0 {
            assert_sometimes!(true, "rpc bootstrap lookup failure stayed distinct");
        }
        let execute = ServiceRef::<Execute>::from_bytes(&listing.execute).ok();
        if self.listings.last().map(|last| last.boot) != Some(listing.boot) {
            self.listings.push(listing);
        }
        execute
    }

    async fn step(
        &mut self,
        op: DeliveryOp,
        execute: &ServiceRef<Execute>,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        monitor: &FailureMonitor<SimProviders>,
    ) {
        let client = execute.bind(rpc);
        match op {
            DeliveryOp::Single => self.single(&client, monitor).await,
            DeliveryOp::SingleTimeout => self.single_timeout(&client, ctx).await,
            DeliveryOp::Reliable => self.reliable(&client, ctx).await,
            DeliveryOp::OneWay => {
                let job = self.job(Mode::Reply, 0);
                let outcome = client.send(&job);
                if outcome.is_ok() {
                    assert_sometimes!(true, "rpc one-way request queued");
                }
                self.record(job.id, op, &outcome);
                if outcome.is_ok()
                    && let Some(call) = self.calls.last_mut()
                {
                    // Queued proves nothing about execution: zero or one.
                    call.class = Class::Maybe;
                }
            }
            DeliveryOp::NoReply => self.no_reply(&client, ctx).await,
            DeliveryOp::Broken => {
                let job = self.job(Mode::Broken, 0);
                let outcome = client.try_get_reply_within(&job, LONG).await;
                assert_always!(outcome.is_err(), "a dropped reply handle never replies");
                if matches!(&outcome, Err(error) if *error.reason() == ErrorReason::BrokenPromise) {
                    assert_sometimes!(true, "rpc broken promise reported to the caller");
                }
                self.record(job.id, op, &outcome);
            }
            DeliveryOp::CallerDrop => self.caller_drop(&client, ctx).await,
            DeliveryOp::Late => self.late(&client, ctx).await,
            DeliveryOp::LostReliable => {
                let job = self.job(Mode::LoseReply, 0);
                let outcome = client
                    .get_reply_unless_failed_for(&job, Duration::from_secs(10), 0.1)
                    .await;
                judge_reply(&job, &outcome);
                self.record(job.id, op, &outcome);
            }
            DeliveryOp::LostSingle => {
                let job = self.job(Mode::LoseReply, 0);
                let outcome = client.try_get_reply_within(&job, LONG).await;
                judge_reply(&job, &outcome);
                self.record(job.id, op, &outcome);
            }
            DeliveryOp::Crash => self.crash(&client, ctx).await,
            DeliveryOp::Stale => self.stale(rpc, monitor).await,
            DeliveryOp::Rediscover => {}
            DeliveryOp::Race => {
                let delay = ctx.random().random_range(0..40);
                let job = self.job(Mode::ReplyThenCrash, delay);
                let outcome = client.try_get_reply_within(&job, LONG).await;
                judge_reply(&job, &outcome);
                match &outcome {
                    Ok(_) => assert_sometimes!(true, "rpc reply won a race with a disconnect"),
                    Err(error) if *error.reason() == ErrorReason::Disconnected => {
                        assert_always!(
                            error.execution() == Execution::MaybeExecuted,
                            "a disconnect after sending is ambiguous"
                        );
                        assert_sometimes!(true, "rpc disconnect won a race with a reply");
                    }
                    Err(_) => {}
                }
                self.dead.push(client.target().to_bytes());
                self.record(job.id, op, &outcome);
            }
            DeliveryOp::Pause => {
                // Long enough for the idle check to see the whole timeout.
                let _ = ctx.time().sleep(self.idle * 2).await;
                self.single(&client, monitor).await;
            }
        }
    }

    async fn single(&mut self, client: &Client, monitor: &FailureMonitor<SimProviders>) {
        let job = self.job(Mode::Reply, 0);
        let address = client.target().endpoint().address();
        let before = monitor.disconnects(address);
        let outcome = client.try_get_reply_within(&job, LONG).await;
        if outcome.is_err() && monitor.disconnects(address) != before {
            assert_reachable!("rpc single attempt met a disconnect");
        }
        judge_reply(&job, &outcome);
        self.record(job.id, DeliveryOp::Single, &outcome);
    }

    async fn single_timeout(&mut self, client: &Client, ctx: &SimContext) {
        let job = self.job(Mode::Slow, ctx.random().random_range(50..400));
        let deadline = Duration::from_millis(ctx.random().random_range(10..200));
        let outcome = client.try_get_reply_within(&job, deadline).await;
        if let Err(error) = &outcome
            && *error.reason() == ErrorReason::Timeout
        {
            assert_sometimes!(
                error.execution() == Execution::MaybeExecuted,
                "rpc timeout after sending is ambiguous"
            );
        }
        self.record(job.id, DeliveryOp::SingleTimeout, &outcome);
    }

    async fn reliable(&mut self, client: &Client, ctx: &SimContext) {
        let job = self.job(Mode::Reply, 0);
        let sustained = Duration::from_millis(ctx.random().random_range(0..3000));
        let outcome = client
            .get_reply_unless_failed_for(&job, sustained, 0.1)
            .await;
        if matches!(&outcome, Err(error) if *error.reason() == ErrorReason::PeerFailed) {
            assert_sometimes!(true, "rpc reliable call bounded by sustained failure");
        }
        judge_reply(&job, &outcome);
        self.record(job.id, DeliveryOp::Reliable, &outcome);
    }

    async fn no_reply(&mut self, client: &Client, ctx: &SimContext) {
        let job = self.job(Mode::NeverReply, 0);
        let deadline = Duration::from_millis(ctx.random().random_range(50..500));
        let outcome = client.try_get_reply_within(&job, deadline).await;
        assert_always!(
            !matches!(&outcome, Err(error) if *error.reason() == ErrorReason::BrokenPromise),
            "an explicit no-reply is never a broken promise"
        );
        assert_always!(outcome.is_err(), "an explicit no-reply never replies");
        if matches!(&outcome, Err(error) if *error.reason() == ErrorReason::Timeout) {
            assert_sometimes!(
                true,
                "rpc explicit no-reply left the caller to its deadline"
            );
        }
        self.record(job.id, DeliveryOp::NoReply, &outcome);
    }

    /// The caller drops the call future (single or reliable) mid-flight.
    async fn caller_drop(&mut self, client: &Client, ctx: &SimContext) {
        let job = self.job(Mode::Slow, ctx.random().random_range(20..300));
        let reliable = ctx.random().random_bool(0.5);
        let patience = Duration::from_millis(ctx.random().random_range(1..150));
        let call = async {
            if reliable {
                client.get_reply(&job).await
            } else {
                client.try_get_reply(&job).await
            }
        };
        let outcome = if let Ok(outcome) = ctx.time().timeout(patience, call).await {
            outcome
        } else {
            assert_reachable!("rpc caller dropped a call in flight");
            Err(RpcError::new(
                ErrorReason::Timeout,
                Execution::MaybeExecuted,
            ))
        };
        let op = if reliable {
            DeliveryOp::Reliable
        } else {
            DeliveryOp::CallerDrop
        };
        self.record(job.id, op, &outcome);
    }

    /// The caller moves on but keeps the attempt, and collects it later.
    async fn late(&mut self, client: &Client, ctx: &SimContext) {
        let job = self.job(Mode::Slow, ctx.random().random_range(20..300));
        let Ok(mut attempt) = client.attempt(&job) else {
            return;
        };
        let patience = Duration::from_millis(ctx.random().random_range(1..100));
        let early = ctx.time().timeout(patience, &mut attempt).await;
        let outcome = if let Ok(outcome) = early {
            outcome
        } else {
            let late = ctx.time().timeout(LONG, attempt).await;
            let outcome = late.unwrap_or_else(|_| {
                Err(RpcError::new(
                    ErrorReason::Timeout,
                    Execution::MaybeExecuted,
                ))
            });
            if outcome.is_ok() {
                assert_sometimes!(true, "rpc late outcome collected by a kept attempt");
            }
            outcome
        };
        judge_reply(&job, &outcome);
        self.record(job.id, DeliveryOp::Late, &outcome);
    }

    /// The server crashes after executing; a reliable call must end at the
    /// dead incarnation, never reach the new one.
    async fn crash(&mut self, client: &Client, ctx: &SimContext) {
        let job = self.job(Mode::Crash, 0);
        // Bounded by sustained failure: a long enough outage ends it as
        // failed, a short one at the restarted (stale) incarnation. A crash
        // request that raced the end of the fault script is never served:
        // the caller's own deadline (a drop) bounds that case.
        let sustained = Duration::from_millis(ctx.random().random_range(0..2000));
        let bounded = client.get_reply_unless_failed_for(&job, sustained, 0.0);
        let outcome = ctx.time().timeout(LONG, bounded).await.unwrap_or_else(|_| {
            Err(RpcError::new(
                ErrorReason::Timeout,
                Execution::MaybeExecuted,
            ))
        });
        assert_always!(
            outcome.is_err(),
            "a reliable call never reaches a fresh incarnation"
        );
        if matches!(&outcome, Err(error) if *error.reason() == ErrorReason::PeerFailed) {
            assert_sometimes!(true, "rpc reliable call bounded by sustained failure");
        }
        if let Err(error) = &outcome
            && *error.reason() == ErrorReason::StaleIncarnation
        {
            assert_always!(
                error.execution() == Execution::MaybeExecuted,
                "a terminal failure after a sent copy keeps its ambiguity"
            );
            assert_sometimes!(
                true,
                "rpc reliable call ended terminally at a stale incarnation"
            );
        }
        if outcome.is_err() {
            self.dead.push(client.target().to_bytes());
        }
        self.record(job.id, DeliveryOp::Crash, &outcome);
        self.directory_while_down(client, ctx).await;
    }

    /// While the crashed server may still be down, ask its well-known
    /// directory reliably, bounded by sustained failure: it either answers
    /// from the new incarnation at the same token or gives up as failed.
    async fn directory_while_down(&mut self, client: &Client, ctx: &SimContext) {
        let address = client.target().endpoint().address();
        let directory = WellKnownRef::<Directory>::new(
            BootstrapAddress::Resolved(address),
            DIRECTORY_ID,
            AccessClass::Public,
        )
        .at(address);
        let Some(rpc) = self.rpc.clone() else {
            return;
        };
        let sustained = Duration::from_millis(ctx.random().random_range(0..1500));
        let outcome = directory
            .bind(&rpc)
            .get_reply_unless_failed_for(&Lookup {}, sustained, 0.0)
            .await;
        match &outcome {
            Ok(_) => assert_sometimes!(true, "rpc well-known endpoint recovered a reliable call"),
            Err(error) if *error.reason() == ErrorReason::PeerFailed => {
                assert_sometimes!(true, "rpc reliable call bounded by sustained failure");
            }
            Err(_) => {}
        }
        self.history
            .push(format!("directory-while-down {}", describe(&outcome)));
    }

    /// Call a reference from an earlier incarnation: it must fail
    /// terminally, and once known, fail without a round trip.
    async fn stale(
        &mut self,
        rpc: &RpcHandle<SimProviders>,
        monitor: &FailureMonitor<SimProviders>,
    ) {
        let current = self.listings.last().map(|listing| listing.boot);
        let Some(old) = self
            .listings
            .iter()
            .find(|listing| Some(listing.boot) != current)
            .and_then(|listing| ServiceRef::<Execute>::from_bytes(&listing.execute).ok())
        else {
            return;
        };
        let job = self.job(Mode::Reply, 0);
        let fast_before = rpc.stats().map_or(0, |stats| stats.calls_failed_fast);
        let known = monitor.endpoint_state(&old.endpoint()).is_permanent();
        let outcome = old
            .bind(rpc)
            .get_reply_unless_failed_for(&job, Duration::from_secs(10), 0.1)
            .await;
        assert_always!(
            outcome.is_err(),
            "a reference from an earlier incarnation never reaches the new one"
        );
        if let Err(error) = &outcome
            && *error.reason() == ErrorReason::StaleIncarnation
        {
            assert_sometimes!(true, "rpc stale dynamic reference failed terminally");
            assert_always!(
                monitor.endpoint_state(&old.endpoint()) == EndpointState::StaleIncarnation
                    || !known,
                "the failure monitor remembers a stale incarnation"
            );
            if known && rpc.stats().map_or(0, |stats| stats.calls_failed_fast) > fast_before {
                assert_sometimes!(true, "rpc known-dead reference failed without a round trip");
            }
        }
        self.record(job.id, DeliveryOp::Stale, &outcome);
    }

    async fn drive(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        bootstrap: &BootstrapClient<SimProviders, ScriptedResolver>,
    ) -> SimulationResult<()> {
        let Some(monitor) = rpc.failure_monitor() else {
            return Ok(());
        };
        let Some(mut execute) = self.discover(bootstrap).await else {
            return Ok(());
        };
        for _ in 0..self.config.operations {
            if ctx.shutdown().is_cancelled() {
                break;
            }
            let mut op = self.pick(ctx);
            if ctx.state().get::<bool>(SCRIPT_DONE_KEY).unwrap_or(false) {
                // Nobody serves crash or cut requests any more.
                op = match op {
                    DeliveryOp::Crash | DeliveryOp::LostReliable => DeliveryOp::Reliable,
                    DeliveryOp::LostSingle | DeliveryOp::Race => DeliveryOp::Single,
                    other => other,
                };
            }
            let rediscover = op == DeliveryOp::Rediscover
                || self.dead.contains(&execute.to_bytes())
                || monitor.endpoint_state(&execute.endpoint()).is_permanent();
            if rediscover && let Some(fresh) = self.discover(bootstrap).await {
                execute = fresh;
            }
            self.step(op, &execute, ctx, rpc, &monitor).await;
            let (low, high) = self.config.gap_ms;
            let gap = ctx.random().random_range(low..high.max(low + 1));
            if ctx.time().sleep(Duration::from_millis(gap)).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

/// A reply must be the reply to this job.
fn judge_reply(job: &Job, outcome: &Result<Done, RpcError>) {
    if let Ok(done) = outcome {
        assert_always!(done.id == job.id, "a delivery reply matches its job");
        assert_always!(done.receipts >= 1, "a reply follows an execution");
    }
}

/// Watch the server address's failure transitions, as a caller would: every
/// disconnect is observed (no lost wakeup), a disconnect is an event before
/// the address fails, and failed addresses recover.
async fn watch_failures(
    ctx: &SimContext,
    monitor: &FailureMonitor<SimProviders>,
    server: SocketAddr,
) {
    let mut failed = false;
    loop {
        // Read the count first: the wait compares against its own later
        // snapshot, so any wakeup means at least this many plus one.
        let seen = monitor.disconnects(server);
        let disconnect = monitor.on_disconnect(server);
        moonpool_sim::select! {
            result = disconnect => {
                if result.is_err() {
                    return;
                }
                assert_always!(
                    monitor.disconnects(server) > seen,
                    "a disconnect wakeup follows a disconnect"
                );
                if monitor.address_state(server) == AddressState::Available {
                    assert_sometimes!(true, "rpc disconnect observed before the address failed");
                }
            }
            result = monitor.on_change() => {
                if result.is_err() {
                    return;
                }
            }
            () = ctx.shutdown().cancelled() => return,
        }
        match monitor.address_state(server) {
            AddressState::Failed if !failed => {
                failed = true;
                assert_sometimes!(true, "rpc failure monitor marked the address failed");
            }
            AddressState::Available if failed => {
                failed = false;
                assert_sometimes!(true, "rpc failed address recovered");
            }
            _ => {}
        }
    }
}

#[async_trait]
impl Workload for DeliveryWorkload {
    fn name(&self) -> &'static str {
        "rpc_delivery_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.state()
            .publish(WORKLOAD_IP_KEY, ctx.my_ip().to_string());
        let resolver = ScriptedResolver::new();
        ctx.state().publish(RESOLVER_KEY, resolver.clone());
        let config = delivery_config();
        self.idle = config.peer.idle_timeout + config.peer.ping_interval * 2;
        let (driver, rpc) = RpcDriver::client_only(ctx.providers().clone(), config)
            .map_err(|error| SimulationError::InvalidState(format!("rpc config: {error}")))?;
        ctx.state().publish(WORKLOAD_RPC_KEY, rpc.clone());
        self.rpc = Some(rpc.clone());
        let board = Board::of(ctx.state());
        let probe = rpc.probe();
        if let Some(probe) = &probe {
            board.register_probe(WORKLOAD_LABEL, probe.clone());
        }
        let bootstrap = BootstrapClient::new(&rpc, resolver, Duration::from_secs(5));
        let server = ctx
            .topology()
            .ips_in_group("server")
            .into_iter()
            .next()
            .and_then(|ip| format!("{ip}:{RPC_PORT}").parse::<SocketAddr>().ok());
        let monitor = rpc.failure_monitor();
        let watcher = async {
            match (monitor.as_ref(), server) {
                (Some(monitor), Some(server)) => watch_failures(ctx, monitor, server).await,
                _ => futures::future::pending().await,
            }
        };
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            result = self.drive(ctx, &rpc, &bootstrap) => result,
            () = watcher => Ok(()),
            () = report_stats(&rpc, &board, WORKLOAD_LABEL, ctx) => Ok(()),
        };
        // Every call finished or was dropped: nothing may stay retained.
        if let Some(stats) = rpc.stats() {
            board.report(WORKLOAD_LABEL, stats);
            assert_always!(
                stats.retained_calls == 0 && stats.pending_calls == 0,
                "no call or retained request outlives its caller"
            );
        }
        drop(monitor);
        drop(bootstrap);
        if let Some(probe) = &probe {
            assert_always!(
                probe.is_released() && probe.monitor_waiters() == 0,
                "the delivery workload released everything once its driver dropped"
            );
        }
        result
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let ledger = Ledger::of(ctx.state());
        let mut duplicates = 0u32;
        for call in &self.calls {
            let receipts = ledger.receipts(call.id);
            if call.op.single_attempt() {
                // The independent external check that nothing retransmits
                // a single attempt, on any path.
                assert_always!(
                    receipts <= 1,
                    "a single attempt never reaches a handler twice"
                );
                if call.lost_in_disconnect && receipts == 1 {
                    assert_sometimes!(true, "rpc reply lost after execution is reported ambiguous");
                }
                if call.class == Class::Executed {
                    assert_always!(receipts == 1, "an executed outcome executed exactly once");
                }
            } else if receipts > 1 {
                duplicates += 1;
            }
            match call.class {
                Class::Replied | Class::Executed => {
                    assert_always!(receipts >= 1, "a call that produced an outcome executed");
                }
                Class::NotAdmitted => {
                    assert_always!(
                        receipts == 0,
                        "a call reported not admitted never reached a handler"
                    );
                }
                Class::Maybe => {}
            }
            tracing::debug!(id = call.id, op = ?call.op, class = ?call.class, receipts, "rpc delivery call judged");
        }
        // Permitted duplicates: reliable delivery may execute twice, and the
        // campaign must show that it does.
        assert_sometimes!(
            duplicates > 0,
            "rpc reliable request executed more than once"
        );
        let workload = Board::of(ctx.state())
            .stats_with_prefix(WORKLOAD_LABEL)
            .first()
            .map(|(_, stats)| *stats)
            .unwrap_or_default();
        assert_sometimes!(
            workload.late_replies > 0,
            "rpc late reply discarded after caller cleanup"
        );
        assert_sometimes!(
            workload.ping_timeouts > 0,
            "rpc ping timeout detected a dead connection"
        );
        assert_sometimes!(workload.idle_closes > 0, "rpc idle connection closed");
        assert_sometimes!(
            workload.reconnect_waits > 0,
            "rpc reconnect waited out the backoff"
        );
        assert_sometimes!(
            workload.retransmissions > 0,
            "rpc retained request retransmitted on a new connection"
        );
        // Quiescence between the two peers: when both share on the claim,
        // each should end up holding exactly the one connection they share.
        let peers = Board::of(ctx.state()).stats_with_prefix("peer#");
        let trusted = ctx
            .topology()
            .ips_in_group("peer")
            .iter()
            .all(|ip| ctx.state().get::<u8>(&super::peer::sharing_key(ip)) == Some(2));
        if trusted && peers.len() == 2 {
            assert_sometimes!(
                peers.iter().all(|(_, stats)| stats.connections == 1),
                "rpc peers agree on one shared connection at the end of the run"
            );
        }
        self.records
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(DeliveryRecord {
                history: std::mem::take(&mut self.history),
                duplicates,
            });
        Ok(())
    }
}
