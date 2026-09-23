//! The end of a run: the drain watcher that runs beside the operations,
//! the recovery phase after the faults stopped, the baselines, and the
//! judgement of every call against the ledgers once late requests landed.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use moonpool_rpc::{ErrorReason, Execution, RpcHandle, ServiceRef};
use moonpool_sim::{SimContext, SimProviders, TimeProvider, assert_always, assert_sometimes};

use super::messages::{Job, Work};
use super::ops::{StreamOutcome, StreamShape};
use super::state::{Instance, QualLedger, SCRIPT_DONE_KEY, instance_code};
use super::workload::{
    CallRecord, Class, Observer, QualOp, QualificationWorkload, Runtimes, describe, model_config,
};
use super::{CALL_TIMEOUT, RECOVERY_BOUND, RELEASE_BOUND, pause};
use crate::foundations::state::Board;
use crate::security::state::{Class as AuthClass, Ledger as AuthLedger};
use crate::security::trust::Kind;
use crate::streams::state::{ProducerEnd, StreamLedger};

/// While the operations run: whenever a server boot starts draining for a
/// graceful shutdown, send it a call at once through the reference we know
/// for that boot. It must be refused without running (admission is
/// closed), or, if the session was already gone, fail without a reply.
pub(crate) async fn drain_watcher(
    ctx: &SimContext,
    rpc: &RpcHandle<SimProviders>,
    done: &AtomicBool,
) -> (Vec<String>, Vec<CallRecord>) {
    let ledger = QualLedger::of(ctx.state());
    let mut lines = Vec::new();
    let mut calls = Vec::new();
    let mut probed = BTreeSet::new();
    // Ids far above the workload's own.
    let mut next_id = 1u64 << 48;
    while !done.load(Ordering::Relaxed) {
        if pause(ctx, Duration::from_millis(20)).await.is_err() {
            break;
        }
        for (process, boot) in ledger.drains() {
            if !probed.insert((process.clone(), boot)) {
                continue;
            }
            let Some(work) = ledger.base_work(&process, boot) else {
                continue;
            };
            let Ok(target) = ServiceRef::<Work>::from_bytes(&work) else {
                continue;
            };
            next_id += 1;
            let id = next_id;
            let expected = Instance {
                process: process.clone(),
                boot,
                configuration: 0,
            };
            let request = Job {
                id,
                server: process.clone(),
                boot,
                configuration: 0,
                hold_ms: 0,
            };
            let outcome = target
                .bind(rpc)
                .try_get_reply_within(&request, CALL_TIMEOUT)
                .await;
            if let Err(error) = &outcome
                && *error.reason() == ErrorReason::ServerShuttingDown
            {
                assert_always!(
                    error.execution() == Execution::NotAdmitted,
                    "rpc qual a shutdown refusal is never admitted"
                );
                assert_sometimes!(true, "rpc qual shutdown refused new admissions");
            }
            let class = super::workload::classify(&outcome);
            lines.push(format!("{id} Drain {expected} {}", describe(&outcome)));
            calls.push(CallRecord {
                id,
                op: QualOp::Unary,
                expected,
                single: true,
                class,
            });
        }
    }
    (lines, calls)
}

/// A lane's end-of-run phase could not finish (the simulation shut it
/// down): the run's recovery or judgement would be skipped silently.
pub(crate) fn cut_short(phase: &str) {
    assert_always!(
        false,
        "rpc qual a lane ran every phase to its end",
        { "phase" => phase }
    );
}

/// One service the recovery phase must regain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Service {
    Unary(String),
    Stream(String),
    Auth(String),
    Balanced,
    Legacy,
    Recruit,
    Callback,
}

impl QualificationWorkload {
    /// After the script stopped: regain every service within
    /// [`RECOVERY_BOUND`], learning every fresh publication explicitly.
    pub(crate) async fn recover(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        for _ in 0..2400 {
            if ctx.state().contains(SCRIPT_DONE_KEY) {
                break;
            }
            if pause(ctx, Duration::from_millis(50)).await.is_err() {
                cut_short("waiting for the fault script");
                return;
            }
        }
        assert_always!(
            ctx.state().contains(SCRIPT_DONE_KEY),
            "rpc qual the fault script ended before the recovery phase"
        );
        let start = ctx.time().now();
        // Everything learned before is suspect: look every process up again.
        self.known.clear();
        let mut missing: BTreeSet<Service> = BTreeSet::new();
        for server in ctx.topology().ips_in_group("server") {
            missing.insert(Service::Unary(server.clone()));
            missing.insert(Service::Stream(server.clone()));
            missing.insert(Service::Auth(server));
        }
        missing.extend([
            Service::Balanced,
            Service::Legacy,
            Service::Recruit,
            Service::Callback,
        ]);
        while !missing.is_empty() && ctx.time().now().saturating_sub(start) < RECOVERY_BOUND {
            for service in missing.clone() {
                if self.regain(ctx, runtimes, &service).await {
                    missing.remove(&service);
                }
            }
            if !missing.is_empty() && pause(ctx, Duration::from_millis(250)).await.is_err() {
                cut_short("recovery");
                return;
            }
        }
        let elapsed = ctx.time().now().saturating_sub(start);
        let recovered = missing.is_empty() && elapsed <= RECOVERY_BOUND;
        assert_always!(
            recovered,
            "rpc qual service recovered within the declared bound",
            { "missing" => format!("{missing:?}"), "elapsed_ms" => elapsed.as_millis() }
        );
        assert_sometimes!(
            recovered,
            "rpc qual every service recovered after the faults"
        );
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        self.recovered_ms = recovered.then_some(elapsed_ms);
        let line = format!("recovered {recovered} in {elapsed_ms} ms");
        tracing::info!(line = %line, "rpc_qual_event");
        self.history.push(line);
    }

    async fn regain(&mut self, ctx: &SimContext, runtimes: &Runtimes, service: &Service) -> bool {
        match service {
            Service::Unary(server) => {
                // A fresh publication, then a call to it.
                if self.known.get(server).is_none_or(|known| {
                    known.boot < QualLedger::of(ctx.state()).current_boot(server)
                }) {
                    self.forget(server);
                    if self.lookup(ctx, &runtimes.rpc, server).await.is_none() {
                        return false;
                    }
                }
                self.unary(ctx, runtimes, Some(server.clone())).await
            }
            Service::Stream(server) => {
                // A new stream, never a resumed one.
                self.stream(ctx, runtimes, Some(server.clone()), StreamShape::Complete)
                    .await
            }
            Service::Auth(server) => {
                self.auth(ctx, runtimes, Some(server.clone()), Kind::Valid)
                    .await
            }
            Service::Balanced => {
                self.refresh(ctx, runtimes).await;
                self.recovery_balanced(ctx, runtimes).await
            }
            Service::Legacy => self.legacy(ctx, runtimes).await,
            Service::Recruit => self.recruit(ctx, runtimes).await,
            Service::Callback => {
                let server = ctx.topology().ips_in_group("server").into_iter().next();
                self.callback(ctx, runtimes, server, false).await
            }
        }
    }

    /// A balanced call with retry permission over the fresh set.
    async fn recovery_balanced(&mut self, ctx: &SimContext, runtimes: &Runtimes) -> bool {
        let id = self.id();
        let request = Job {
            id,
            server: String::new(),
            boot: 0,
            configuration: 0,
            hold_ms: 0,
        };
        let policy = moonpool_rpc::balance::BalancePolicy {
            retry: moonpool_rpc::balance::Retry::AfterAmbiguous,
            attempt_timeout: Some(Duration::from_millis(1500)),
            ..moonpool_rpc::balance::BalancePolicy::default()
        };
        let primaries = Observer::load(&self.observer.primaries);
        let copies = Observer::load(&self.observer.copies);
        let outcome = runtimes.balanced.call(request, &policy).await;
        let check = super::ops::BalancedCheck {
            id,
            policy_cap: super::ops::policy_cap(&policy),
            permitted: Observer::load(&self.observer.primaries) - primaries
                + Observer::load(&self.observer.copies)
                - copies,
            not_admitted: matches!(&outcome, Err(error) if error.execution() == Execution::NotAdmitted),
            instances: self.set_instances.clone(),
        };
        check.verify(&QualLedger::of(ctx.state()));
        self.balanced.push(check);
        let line = match &outcome {
            Ok(done) => format!(
                "{id} Balanced recovery ok {}#{}",
                done.reply.server, done.reply.boot
            ),
            Err(error) => format!("{id} Balanced recovery err {:?}", error.execution()),
        };
        tracing::info!(line = %line, "rpc_qual_event");
        self.history.push(line);
        outcome.is_ok()
    }

    /// Every late loser of this lane's balancer ends within its bounds;
    /// then its attempt ledger and its queue model agree.
    pub(crate) async fn drain_balancer(&self, ctx: &SimContext, runtimes: &Runtimes) {
        let model = runtimes.balanced.model();
        for _ in 0..200 {
            let stats = model.stats();
            if stats.in_flight == 0 && stats.lagging == 0 {
                break;
            }
            if pause(ctx, Duration::from_millis(50)).await.is_err() {
                cut_short("draining the balancer");
                return;
            }
        }
        let stats = model.stats();
        let started = Observer::load(&self.observer.started);
        let ended = Observer::load(&self.observer.ended);
        assert_always!(
            stats.in_flight == 0 && stats.lagging == 0 && started == ended,
            "rpc qual every late loser ended within its bounds",
            { "in_flight" => stats.in_flight, "lagging" => stats.lagging, "started" => started, "ended" => ended }
        );
        assert_always!(
            stats.reservations == started && stats.releases == stats.reservations,
            "rpc qual the queue model released each reservation once",
            { "reservations" => stats.reservations, "releases" => stats.releases, "started" => started }
        );
        assert_always!(
            stats.lagging <= model_config().max_lagging,
            "late losers stay within the model's bound"
        );
        assert_sometimes!(
            stats.lagging_completed > 0 && Observer::load(&self.observer.ended_late) > 0,
            "rpc qual late loser completed after its call returned"
        );
        assert_sometimes!(stats.copies_denied > 0, "rpc qual hedge budget exhausted");
    }

    /// This lane's client runtime is idle: no call, retained request,
    /// stream, window or buffered byte left.
    pub(crate) fn client_idle(&self, runtimes: &Runtimes) {
        let client = runtimes.rpc.stats().unwrap_or_default();
        let outstanding = runtimes
            .rpc
            .probe()
            .map(|probe| probe.outstanding())
            .unwrap_or_default();
        assert_always!(
            client.pending_calls == 0
                && client.retained_calls == 0
                && client.streams_consuming == 0
                && outstanding.is_empty(),
            "rpc qual client runtime returned to its idle baseline",
            { "lane" => self.lane, "stats" => format!("{client:?}"), "outstanding" => format!("{outstanding:?}") }
        );
    }

    /// The lead lane's idle baseline once every lane stopped: every stream a
    /// current boot produced ended within [`RELEASE_BOUND`], then (after
    /// late requests landed) its client runtime and every serving runtime
    /// are idle.
    pub(crate) async fn quiesce(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        // Streams of the current boots: an abandoned stream must release
        // its producer within the declared bound.
        let streams = StreamLedger::of(ctx.state());
        let ledger = QualLedger::of(ctx.state());
        let deadline = ctx.time().now() + RELEASE_BOUND;
        let open = || {
            let current: BTreeSet<u64> = ctx
                .topology()
                .ips_in_group("server")
                .iter()
                .map(|server| instance_code(server, ledger.current_boot(server)))
                .collect();
            streams
                .all_produced()
                .into_iter()
                .filter(|(_, produced)| current.contains(&produced.boot) && produced.end.is_none())
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        while !open().is_empty() && ctx.time().now() < deadline {
            if pause(ctx, Duration::from_millis(50)).await.is_err() {
                cut_short("waiting for streams to end");
                return;
            }
        }
        let still = open();
        assert_always!(
            still.is_empty(),
            "rpc qual no stream outlived its consumer beyond the bound",
            { "open" => format!("{still:?}") }
        );
        // Requests of cancelled calls and dropped late losers may still land.
        if pause(ctx, Duration::from_secs(3)).await.is_err() {
            cut_short("settling before the baseline");
            return;
        }
        self.client_idle(runtimes);
        // Every running server's live runtime, read through its probe (not
        // a periodic report): nothing owed, produced, queued or reserved.
        let board = Board::of(ctx.state());
        for server in ctx.topology().ips_in_group("server") {
            let label = format!("server@{server}#{:04}", ledger.current_boot(&server));
            let probes = board.probes(&label, "");
            assert_always!(
                !probes.is_empty(),
                "rpc qual every running server registered its probe"
            );
            for (_, probe) in probes {
                let outstanding = probe.outstanding();
                assert_always!(
                    outstanding.is_empty(),
                    "rpc qual every serving runtime returned to its idle baseline",
                    { "runtime" => label.clone(), "outstanding" => format!("{outstanding:?}") }
                );
            }
        }
    }

    /// Judge every recorded call, stream, balanced call and credentialed
    /// request against the independent ledgers.
    pub(crate) fn judge_all(&self, ctx: &SimContext) {
        let ledger = QualLedger::of(ctx.state());
        for call in &self.calls {
            let runs = ledger.executions(call.id);
            for run in &runs {
                assert_always!(
                    *run == call.expected,
                    "rpc qual every execution was in the instance its call named",
                    { "id" => call.id, "op" => format!("{:?}", call.op), "expected" => call.expected.to_string(), "ran" => run.to_string() }
                );
            }
            match call.class {
                Class::Replied | Class::Executed => {
                    assert_always!(
                        !runs.is_empty(),
                        "rpc qual a replied call executed",
                        { "id" => call.id, "op" => format!("{:?}", call.op) }
                    );
                }
                Class::NotAdmitted => {
                    assert_always!(
                        runs.is_empty(),
                        "rpc qual a not-admitted call never executed",
                        { "id" => call.id, "op" => format!("{:?}", call.op) }
                    );
                }
                Class::Maybe => {}
            }
            if call.single {
                assert_always!(
                    runs.len() <= 1,
                    "rpc qual a single attempt never executed twice",
                    { "id" => call.id, "op" => format!("{:?}", call.op), "runs" => runs.len() }
                );
            }
        }
        for check in &self.balanced {
            check.verify(&ledger);
        }
        let streams = StreamLedger::of(ctx.state());
        for check in &self.streams {
            judge_stream(&streams, check);
        }
        // Credentialed requests: the security campaign's end-of-run rules.
        for (id, issued, receipts, class) in AuthLedger::of(ctx.state()).all() {
            match class {
                Some(AuthClass::Replied | AuthClass::Executed) => {
                    assert_always!(
                        receipts >= 1,
                        "a request that produced an outcome reached its handler",
                        { "id" => id }
                    );
                }
                Some(AuthClass::NotAdmitted) => {
                    assert_always!(
                        receipts == 0,
                        "a request reported not admitted never reached a handler",
                        { "id" => id }
                    );
                }
                Some(AuthClass::Maybe) | None => {}
            }
            if issued.minted.kind != Kind::Refreshing {
                assert_always!(receipts <= 1, "an at-most-once request ran at most once");
            }
        }
    }
}

/// Judge one stream against the producer and consumer ledgers.
fn judge_stream(ledger: &StreamLedger, check: &super::ops::StreamCheck) {
    let consumed = ledger.consumed(check.id);
    let produced = ledger.produced(check.id);
    if let Some(produced) = &produced {
        assert_always!(
            produced.boot == check.expected,
            "rpc qual a stream ran only in the incarnation it named",
            { "id" => check.id, "expected" => check.expected, "ran" => produced.boot }
        );
        assert_always!(
            consumed.items.len() <= produced.items.len(),
            "a consumer never takes more items than its producer sent"
        );
        for (position, (seq, size, boot)) in consumed.items.iter().enumerate() {
            assert_always!(
                *seq == u64::try_from(position).unwrap_or(u64::MAX)
                    && produced.items.get(position) == Some(size)
                    && *boot == produced.boot,
                "rpc qual each consumed item is the produced one at its place"
            );
        }
        if produced.waited && produced.end == Some(ProducerEnd::Finished) {
            assert_sometimes!(true, "rpc qual producer exhausted credit and resumed");
            if check.shape == StreamShape::Slow {
                assert_sometimes!(
                    true,
                    "rpc qual slow consumer held the producer to its window"
                );
            }
        }
    } else {
        assert_always!(
            consumed.items.is_empty(),
            "stream items come only from a producer that served the stream"
        );
    }
    if let StreamOutcome::Failed(error) = &check.outcome
        && *error.reason() == ErrorReason::Disconnected
        && !consumed.items.is_empty()
    {
        assert_sometimes!(true, "rpc qual stream ended by a disconnect mid-stream");
    }
    let all_items = produced
        .as_ref()
        .is_some_and(|produced| produced.items.len() == consumed.items.len());
    let end = produced.as_ref().and_then(|produced| produced.end);
    match &check.outcome {
        StreamOutcome::Normal => {
            assert_always!(
                end == Some(ProducerEnd::Finished) && all_items,
                "a normal end delivered every item its producer sent"
            );
        }
        StreamOutcome::Failed(error) => match error.reason() {
            ErrorReason::StreamFailed { code } => {
                assert_always!(
                    end == Some(ProducerEnd::Failed(*code)) && all_items,
                    "a producer's failure arrives after every item it sent"
                );
            }
            ErrorReason::BrokenPromise => {
                assert_always!(
                    produced.is_none()
                        || (matches!(end, Some(ProducerEnd::Dropped | ProducerEnd::TooLarge))
                            && all_items),
                    "a broken promise follows every item of a dropped producer"
                );
            }
            ErrorReason::StreamProtocol(_) => {
                assert_always!(false, "no stream protocol violation between honest peers");
            }
            _ if error.execution() == Execution::NotAdmitted => {
                assert_always!(
                    produced.is_none(),
                    "a stream refused before admission never reached a producer"
                );
            }
            _ => {}
        },
        StreamOutcome::NotOpened(_) => {
            assert_always!(
                produced.is_none(),
                "a stream that failed to open never reached a producer"
            );
        }
        StreamOutcome::Abandoned => {}
    }
}

/// No credential a caller minted appears in any traced field.
pub(crate) fn check_redaction(ctx: &SimContext, trace: &[String]) {
    let tokens: Vec<String> = AuthLedger::of(ctx.state())
        .all()
        .into_iter()
        .filter_map(|(_, issued, _, _)| issued.minted.token)
        .filter_map(|token| String::from_utf8(token).ok())
        // The signature segment: never a prefix of anything else.
        .filter_map(|token| token.rsplit('.').next().map(ToString::to_string))
        .filter(|signature| signature.len() >= 16)
        .collect();
    let leaked = trace
        .iter()
        .any(|line| tokens.iter().any(|token| line.contains(token.as_str())));
    assert_always!(!leaked, "rpc qual no credential appeared in a trace field");
    assert_sometimes!(
        trace.iter().any(|line| line.contains("rpc_request_denied")),
        "rpc qual a denial was audited in the trace"
    );
}
