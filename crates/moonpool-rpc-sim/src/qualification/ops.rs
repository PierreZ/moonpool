//! The workload's operations. Each judges what it can on the spot and
//! records what must be judged again once every late request has landed.

use std::time::Duration;

use futures::StreamExt;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use moonpool_rpc::balance::{
    AttemptKind, AttemptOutcome, BalanceError, BalanceFailure, BalancePolicy, Balanced,
    DuplicatePolicy, Duplicates, HedgeTiming, Retry,
};
use moonpool_rpc::protocol::stream_item_frame_len;
use moonpool_rpc::{
    AccessClass, BootstrapAddress, ErrorReason, Execution, ReplyStream, RpcError, RpcHandle,
    ServiceRef, WellKnownRef,
};
use moonpool_sim::{
    NetworkProvider, RandomProvider, SimContext, SimProviders, TimeProvider, assert_always,
    assert_sometimes,
};

use super::messages::{
    ADOPTER_ID, Adopt, Adopter, Done, FORWARDER_ID, Forward, Forwarder, Job, Publication,
    RECRUITER_ID, Recruit, Recruiter, Relayed, Work,
};
use super::state::{CUT_REQUESTS_KEY, Instance, QualLedger, instance_code};
use super::workload::{Class, Observer, QualOp, QualificationWorkload, Runtimes, describe};
use super::{CALL_TIMEOUT, RPC_PORT, pause};
use crate::security::messages::PrivateEcho;
use crate::security::state::Target;
use crate::security::trust::Kind;
use crate::streams::messages::{End, SHUTDOWN_CODE, Scan, ScanItems};

/// How a stream is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamShape {
    /// Take every item as it comes.
    Eager,
    /// A window of two items, a pause after each item.
    Slow,
    /// Give up before the first item.
    AbandonBefore,
    /// Give up after one to three items.
    AbandonAfter,
    /// An endless stream read until the caller has had enough.
    Endless,
    /// Take every item of a stream that ends normally (recovery).
    Complete,
}

/// How a stream ended for its consumer.
#[derive(Debug, Clone)]
pub(crate) enum StreamOutcome {
    /// A normal end after every item.
    Normal,
    /// A terminal error after the items taken.
    Failed(RpcError),
    /// The consumer dropped it.
    Abandoned,
    /// Refused before it opened.
    NotOpened(RpcError),
}

/// One stream to judge against the producer and consumer ledgers.
#[derive(Debug, Clone)]
pub(crate) struct StreamCheck {
    pub(crate) id: u64,
    pub(crate) shape: StreamShape,
    /// The instance code of the boot its reference named.
    pub(crate) expected: u64,
    pub(crate) outcome: StreamOutcome,
}

/// One balanced call to judge against the execution ledger.
#[derive(Debug, Clone)]
pub(crate) struct BalancedCheck {
    pub(crate) id: u64,
    /// Executions its policy alone allows, whatever the balancer did:
    /// at most one sequential attempt may run without retry permission
    /// (`max_attempts` with it), plus `max_copies` concurrent copies.
    pub(crate) policy_cap: u64,
    /// A tighter bound from the attempts the observation hook reported
    /// (the balancer's own account, so only an additional check).
    pub(crate) permitted: u64,
    /// It reported that nothing ran.
    pub(crate) not_admitted: bool,
    /// The set's instances: the only ones that may run it.
    pub(crate) instances: Vec<Instance>,
}

/// The executions `policy` allows one balanced call, from the policy alone.
pub(crate) fn policy_cap(policy: &BalancePolicy) -> u64 {
    let sequential = match policy.retry {
        Retry::AtMostOnce => 1,
        Retry::AfterAmbiguous => u64::from(policy.max_attempts.max(1)),
    };
    let copies = match &policy.duplicates {
        Duplicates::Permitted(copies) => u64::from(copies.max_copies),
        Duplicates::Forbidden => 0,
    };
    sequential + copies
}

impl BalancedCheck {
    pub(crate) fn verify(&self, ledger: &QualLedger) {
        let runs = ledger.executions(self.id);
        let executed = u64::try_from(runs.len()).unwrap_or(u64::MAX);
        assert_always!(
            executed <= self.policy_cap,
            "rpc qual a balanced job ran at most as often as its policy allows",
            { "id" => self.id, "executed" => executed, "cap" => self.policy_cap }
        );
        assert_always!(
            executed <= self.permitted,
            "rpc qual a balanced job ran at most as often as permitted",
            { "id" => self.id, "executed" => executed, "permitted" => self.permitted }
        );
        if self.not_admitted {
            assert_always!(
                executed == 0,
                "rpc qual a not-admitted call never executed",
                { "id" => self.id }
            );
        }
        for run in &runs {
            assert_always!(
                self.instances.contains(run),
                "rpc qual a balanced job ran only in an incarnation of its set",
                { "id" => self.id, "ran" => run.to_string() }
            );
        }
    }
}

/// Balanced call permissions, as drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permission {
    AtMostOnce,
    Retry,
    Hedged,
    Cancel,
    /// Hedged, through the client that carries credentials.
    Credentialed,
}

impl Permission {
    fn policy(self) -> BalancePolicy {
        let timeout = Some(Duration::from_millis(1500));
        match self {
            Self::AtMostOnce => BalancePolicy {
                attempt_timeout: timeout,
                ..BalancePolicy::default()
            },
            Self::Retry => BalancePolicy {
                retry: Retry::AfterAmbiguous,
                attempt_timeout: timeout,
                ..BalancePolicy::default()
            },
            Self::Hedged | Self::Cancel | Self::Credentialed => BalancePolicy {
                duplicates: Duplicates::Permitted(DuplicatePolicy {
                    max_copies: 1,
                    hedge: Some(HedgeTiming::default()),
                    compare_within: Duration::from_secs(1),
                }),
                attempt_timeout: Some(Duration::from_secs(2)),
                ..BalancePolicy::default()
            },
        }
    }
}

fn instance_of(publication: &Publication) -> Instance {
    Instance {
        process: publication.server.clone(),
        boot: publication.boot,
        configuration: publication.configuration,
    }
}

fn job(id: u64, expected: &Instance, hold_ms: u32) -> Job {
    Job {
        id,
        server: expected.process.clone(),
        boot: expected.boot,
        configuration: expected.configuration,
        hold_ms,
    }
}

/// Whether an error says the reference's incarnation is gone for good.
fn is_stale(error: &RpcError) -> bool {
    matches!(
        error.reason(),
        ErrorReason::StaleIncarnation | ErrorReason::EndpointNotFound
    )
}

/// Parse the execution a relay reported (`id:reason/execution`).
fn relayed_class(failure: &str) -> Class {
    if failure.ends_with("/NotAdmitted") {
        Class::NotAdmitted
    } else if failure.ends_with("/Executed") {
        Class::Executed
    } else {
        Class::Maybe
    }
}

impl QualificationWorkload {
    pub(crate) async fn step(&mut self, op: QualOp, ctx: &SimContext, runtimes: &Runtimes) {
        match op {
            QualOp::Unary => {
                let _ = self.unary(ctx, runtimes, None).await;
            }
            QualOp::Reliable => self.reliable(ctx, runtimes, false).await,
            QualOp::UnlessFailed => self.reliable(ctx, runtimes, true).await,
            QualOp::OneWay => self.one_way(ctx, runtimes).await,
            QualOp::Stream => {
                let shape = [
                    StreamShape::Eager,
                    StreamShape::Slow,
                    StreamShape::AbandonBefore,
                    StreamShape::AbandonAfter,
                    StreamShape::Endless,
                ][ctx.random().random_range(0..5)];
                let _ = self.stream(ctx, runtimes, None, shape).await;
            }
            QualOp::Balanced => {
                let permission = [
                    Permission::AtMostOnce,
                    Permission::Retry,
                    Permission::Hedged,
                    Permission::Hedged,
                    Permission::Cancel,
                    Permission::Credentialed,
                ][ctx.random().random_range(0..6)];
                let _ = self.balanced_call(ctx, runtimes, permission).await;
            }
            QualOp::Auth => {
                let kind = [
                    Kind::Valid,
                    Kind::Valid,
                    Kind::ShortLived,
                    Kind::ShortLived,
                    Kind::RotatedOut,
                    Kind::RotatedOut,
                    Kind::Anonymous,
                    Kind::Forged,
                    Kind::WrongAudience,
                ][ctx.random().random_range(0..9)];
                let _ = self.auth(ctx, runtimes, None, kind).await;
            }
            QualOp::Stale => self.stale(ctx, runtimes).await,
            QualOp::Lookup => {
                if let Some(server) = Self::random_server(ctx) {
                    let _ = self.lookup(ctx, &runtimes.rpc, &server).await;
                }
            }
            QualOp::Recruit => {
                let _ = self.recruit(ctx, runtimes).await;
            }
            QualOp::Callback => {
                let stale = ctx.random().random_bool(0.5);
                let _ = self.callback(ctx, runtimes, None, stale).await;
            }
            QualOp::Legacy => {
                let _ = self.legacy(ctx, runtimes).await;
            }
            QualOp::Version1 => self.version1(ctx, runtimes).await,
            QualOp::Burst => self.burst(ctx, runtimes).await,
            QualOp::Malformed => self.malformed(ctx, runtimes).await,
            QualOp::Shutdown => self.shutdown_under_work(ctx, runtimes).await,
        }
    }

    /// The target server's known publication and its `Work` client.
    async fn work_target(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        server: Option<String>,
    ) -> Option<(Instance, ServiceRef<Work>)> {
        let server = server.or_else(|| Self::random_server(ctx))?;
        let publication = self.publication(ctx, rpc, &server).await?;
        let target = ServiceRef::<Work>::from_bytes(&publication.work).ok()?;
        Some((instance_of(&publication), target))
    }

    /// Error gates every unary-like call shares; forgets a stale reference.
    /// `single`: the call was one attempt (a reliable call may have sent
    /// earlier copies before the refused one).
    fn judge_error(
        &mut self,
        ctx: &SimContext,
        id: u64,
        expected: &Instance,
        error: &RpcError,
        single: bool,
    ) {
        let ledger = QualLedger::of(ctx.state());
        let executed = !ledger.executions(id).is_empty();
        match (error.reason(), error.execution()) {
            (ErrorReason::Timeout, Execution::MaybeExecuted) if executed => {
                assert_sometimes!(true, "rpc qual timeout after execution");
            }
            (ErrorReason::Disconnected, Execution::MaybeExecuted) if executed => {
                assert_sometimes!(
                    true,
                    "rpc qual reply lost after execution reported ambiguous"
                );
            }
            (ErrorReason::Overloaded, Execution::NotAdmitted) => {
                assert_sometimes!(true, "rpc qual request refused overloaded before admission");
            }
            (ErrorReason::ServerShuttingDown, execution) => {
                // A refusal proves non-admission of its own attempt; a
                // reliable call's earlier copy may still have run.
                assert_always!(
                    execution == Execution::NotAdmitted
                        || (!single && execution == Execution::MaybeExecuted),
                    "rpc qual a shutdown refusal is never admitted",
                    { "id" => id, "execution" => format!("{execution:?}"), "single" => single }
                );
                if execution == Execution::NotAdmitted {
                    assert_sometimes!(true, "rpc qual shutdown refused new admissions");
                } else {
                    assert_sometimes!(
                        true,
                        "rpc qual resent call refused while draining, earlier copy left"
                    );
                }
            }
            (ErrorReason::StaleIncarnation, _)
                if ledger.current_boot(&expected.process) > expected.boot =>
            {
                assert_sometimes!(
                    true,
                    "rpc qual stale I1 refused while I2 served at the same address"
                );
            }
            _ => {}
        }
        if is_stale(error) {
            self.forget(&expected.process);
        }
    }

    /// Check a reply names the instance the reference named.
    fn judge_reply(ctx: &SimContext, id: u64, expected: &Instance, done: &Done) {
        assert_always!(
            done.id == id
                && done.server == expected.process
                && done.boot == expected.boot
                && done.configuration == expected.configuration,
            "rpc qual a reply came from the instance its reference named",
            { "id" => id, "expected" => expected.to_string() }
        );
        if QualLedger::of(ctx.state()).ended(&expected.process, expected.boot) {
            assert_sometimes!(true, "rpc qual reply won a race with its server going down");
        }
    }

    /// An at-most-once call; returns whether it replied.
    pub(crate) async fn unary(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        server: Option<String>,
    ) -> bool {
        let recovery = server.is_some();
        let Some((expected, target)) = self.work_target(ctx, &runtimes.rpc, server).await else {
            return false;
        };
        let id = self.id();
        let hold = if recovery {
            0
        } else {
            ctx.random().random_range(0..600u32)
        };
        let client = target.bind(&runtimes.rpc);
        let request = job(id, &expected, hold);
        let give_up = !recovery && ctx.random().random_bool(0.15);
        // Some callers set a deadline shorter than the work.
        let deadline = if !recovery && ctx.random().random_bool(0.2) {
            Duration::from_millis(ctx.random().random_range(50..500))
        } else {
            CALL_TIMEOUT
        };
        let outcome = if give_up {
            let patience = Duration::from_millis(ctx.random().random_range(1..300));
            moonpool_sim::select! {
                outcome = client.try_get_reply_within(&request, deadline) => Some(outcome),
                _ = pause(ctx, patience) => None,
            }
        } else {
            Some(client.try_get_reply_within(&request, deadline).await)
        };
        let Some(outcome) = outcome else {
            assert_sometimes!(true, "rpc qual caller dropped a call in flight");
            self.record_class(
                id,
                QualOp::Unary,
                &expected,
                true,
                Class::Maybe,
                "abandoned",
            );
            return false;
        };
        match &outcome {
            Ok(done) => Self::judge_reply(ctx, id, &expected, done),
            Err(error) => self.judge_error(ctx, id, &expected, error, true),
        }
        self.record(id, QualOp::Unary, &expected, true, &outcome) == Class::Replied
    }

    /// A reliable call (or one bounded by sustained failure), sometimes
    /// with a cut asked for while it is held, and sometimes beside a
    /// stream to the same server.
    async fn reliable(&mut self, ctx: &SimContext, runtimes: &Runtimes, bounded: bool) {
        let Some((expected, target)) = self.work_target(ctx, &runtimes.rpc, None).await else {
            return;
        };
        let id = self.id();
        let request = job(id, &expected, ctx.random().random_range(200..1500));
        if bounded || ctx.random().random_bool(0.6) {
            let mut asked = ctx
                .state()
                .get::<Vec<(String, String)>>(CUT_REQUESTS_KEY)
                .unwrap_or_default();
            asked.push((ctx.my_ip().to_string(), expected.process.clone()));
            ctx.state().publish(CUT_REQUESTS_KEY, asked);
        }
        let beside = ctx.random().random_bool(0.4);
        let client = target.bind(&runtimes.rpc);
        let call = async {
            let reply = async {
                if bounded {
                    let sustained = Duration::from_millis(ctx.random().random_range(300..2000));
                    client
                        .get_reply_unless_failed_for(&request, sustained, 0.1)
                        .await
                } else {
                    client.get_reply(&request).await
                }
            };
            moonpool_sim::select! {
                outcome = reply => Some(outcome),
                _ = pause(ctx, Duration::from_secs(15)) => None,
            }
        };
        let (outcome, beside) = if beside {
            let server = Some(expected.process.clone());
            futures::join!(call, self.stream_beside(ctx, runtimes, server))
        } else {
            (call.await, None)
        };
        let streamed = beside.as_ref().is_some_and(|check| {
            !crate::streams::state::StreamLedger::of(ctx.state())
                .consumed(check.id)
                .items
                .is_empty()
        });
        if let Some(check) = beside {
            self.history
                .push(format!("{} Stream beside {}", check.id, id));
            self.streams.push(check);
        }
        let op = if bounded {
            QualOp::UnlessFailed
        } else {
            QualOp::Reliable
        };
        let Some(outcome) = outcome else {
            self.record_class(id, op, &expected, false, Class::Maybe, "abandoned");
            return;
        };
        let runs = QualLedger::of(ctx.state()).executions(id).len();
        match &outcome {
            Ok(done) => {
                Self::judge_reply(ctx, id, &expected, done);
                if runs > 1 {
                    assert_sometimes!(true, "rpc qual reliable request executed more than once");
                    if streamed {
                        assert_sometimes!(
                            true,
                            "rpc qual reliable duplicate while a stream shared the session"
                        );
                    }
                }
            }
            Err(error) => {
                if *error.reason() == ErrorReason::StaleIncarnation
                    && QualLedger::of(ctx.state()).current_boot(&expected.process) > expected.boot
                {
                    assert_sometimes!(
                        true,
                        "rpc qual retained reliable call to I1 ended terminally"
                    );
                }
                if *error.reason() == ErrorReason::PeerFailed {
                    assert_sometimes!(true, "rpc qual reliable call bounded by sustained failure");
                }
                self.judge_error(ctx, id, &expected, error, false);
            }
        }
        self.record(id, op, &expected, false, &outcome);
    }

    /// A slow stream opened beside a held reliable call, consumed to its
    /// end; returns its check (judged against the incarnation its
    /// reference named, like every other stream) once it ended.
    async fn stream_beside(
        &self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        server: Option<String>,
    ) -> Option<StreamCheck> {
        let publication = self.known.get(&server?).cloned()?;
        let target = ServiceRef::<ScanItems>::from_bytes(&publication.scan).ok()?;
        // Its own id space (the high bit), apart from the lanes' ids.
        let id = (1 << 63) | ctx.random().random_range(0..(1u64 << 40));
        let scan = Scan {
            id,
            count: 8,
            item_bytes: 64,
            first_delay_ms: 0,
            delay_ms: 100,
            end: End::Finish as u32,
            code: 0,
        };
        let outcome = match target.bind(&runtimes.rpc).get_reply_stream(&scan) {
            Err(error) => StreamOutcome::NotOpened(error),
            Ok(stream) => self.consume(ctx, stream, id, StreamShape::Complete).await,
        };
        Some(StreamCheck {
            id,
            shape: StreamShape::Complete,
            expected: instance_code(&publication.server, publication.boot),
            outcome,
        })
    }

    async fn one_way(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        let Some((expected, target)) = self.work_target(ctx, &runtimes.rpc, None).await else {
            return;
        };
        let id = self.id();
        match target.bind(&runtimes.rpc).send(&job(id, &expected, 0)) {
            // Queued proves nothing: zero or one delivery.
            Ok(()) => {
                self.record_class(id, QualOp::OneWay, &expected, true, Class::Maybe, "queued");
            }
            Err(error) => {
                self.record(id, QualOp::OneWay, &expected, true, &Err::<(), _>(error));
            }
        }
    }

    /// Open and consume a stream of the given shape; returns whether it
    /// ended normally after every item.
    pub(crate) async fn stream(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        server: Option<String>,
        shape: StreamShape,
    ) -> bool {
        let Some(server) = server.or_else(|| Self::random_server(ctx)) else {
            return false;
        };
        let Some(publication) = self.publication(ctx, &runtimes.rpc, &server).await else {
            return false;
        };
        let Ok(target) = ServiceRef::<ScanItems>::from_bytes(&publication.scan) else {
            return false;
        };
        let id = self.id();
        let scan = Self::plan_stream(ctx, id, shape);
        let window = scan.window;
        let scan = scan.scan;
        let expected = instance_code(&publication.server, publication.boot);
        let outcome = match target
            .bind(&runtimes.rpc)
            .get_reply_stream_with_window(&scan, window)
        {
            Err(error) => {
                assert_always!(
                    error.execution() == Execution::NotAdmitted,
                    "a stream that failed to open was never admitted"
                );
                StreamOutcome::NotOpened(error)
            }
            Ok(stream) => self.consume(ctx, stream, id, shape).await,
        };
        let ledger = crate::streams::state::StreamLedger::of(ctx.state());
        let taken = ledger.consumed(id).items.len();
        match (&outcome, shape) {
            (StreamOutcome::Abandoned, StreamShape::AbandonBefore) if taken == 0 => {
                assert_sometimes!(true, "rpc qual stream abandoned before its first item");
            }
            (StreamOutcome::Abandoned, StreamShape::AbandonAfter) if taken > 0 => {
                assert_sometimes!(true, "rpc qual stream abandoned after its first item");
            }
            (StreamOutcome::Failed(error), _) => match error.reason() {
                ErrorReason::StreamFailed { code } if *code == SHUTDOWN_CODE => {
                    assert_sometimes!(true, "rpc qual stream failed by a graceful shutdown");
                }
                ErrorReason::Disconnected if taken > 0 => {
                    assert_sometimes!(true, "rpc qual stream ended by a disconnect mid-stream");
                }
                _ if is_stale(error) => self.forget(&server),
                _ => {}
            },
            (StreamOutcome::NotOpened(error), _) if is_stale(error) => self.forget(&server),
            _ => {}
        }
        let normal = matches!(outcome, StreamOutcome::Normal);
        let line = format!(
            "{id} Stream {shape:?} {}#{} {taken} {}",
            publication.server,
            publication.boot,
            match &outcome {
                StreamOutcome::Normal => "normal".to_string(),
                StreamOutcome::Abandoned => "abandoned".to_string(),
                StreamOutcome::Failed(error) =>
                    format!("failed {:?}/{:?}", error.reason(), error.execution()),
                StreamOutcome::NotOpened(error) =>
                    format!("not-opened {:?}/{:?}", error.reason(), error.execution()),
            }
        );
        tracing::info!(line = %line, "rpc_qual_event");
        self.history.push(line);
        self.streams.push(StreamCheck {
            id,
            shape,
            expected,
            outcome,
        });
        normal
    }

    /// Take items into the consumer ledger as the shape says.
    async fn consume(
        &self,
        ctx: &SimContext,
        mut stream: ReplyStream<ScanItems>,
        id: u64,
        shape: StreamShape,
    ) -> StreamOutcome {
        let ledger = crate::streams::state::StreamLedger::of(ctx.state());
        let limit = match shape {
            StreamShape::AbandonBefore => Some(0),
            StreamShape::AbandonAfter => Some(ctx.random().random_range(1..4u64)),
            StreamShape::Endless => Some(ctx.random().random_range(3..12u64)),
            _ => None,
        };
        let slow = Duration::from_millis(ctx.random().random_range(20..80));
        let patience = if shape == StreamShape::AbandonBefore {
            Duration::from_millis(ctx.random().random_range(5..150))
        } else {
            Duration::from_secs(10)
        };
        let mut taken = 0u64;
        loop {
            if limit.is_some_and(|limit| taken >= limit) && shape != StreamShape::AbandonBefore {
                return StreamOutcome::Abandoned;
            }
            let Ok(next) = ctx.time().timeout(patience, stream.next()).await else {
                return StreamOutcome::Abandoned;
            };
            match next {
                None => return StreamOutcome::Normal,
                Some(Err(error)) => {
                    assert_always!(
                        stream.next().await.is_none(),
                        "rpc qual a stream has exactly one terminal outcome"
                    );
                    return StreamOutcome::Failed(error);
                }
                Some(Ok(chunk)) => {
                    assert_always!(
                        chunk.id == id && chunk.seq == taken,
                        "stream items arrive in order, without gaps or repeats"
                    );
                    let size = stream_item_frame_len(prost::Message::encoded_len(&chunk));
                    ledger.consume(id, chunk.seq, size, chunk.boot);
                    taken += 1;
                    if shape == StreamShape::AbandonBefore {
                        // The first item beat the caller's patience.
                        return StreamOutcome::Abandoned;
                    }
                    if shape == StreamShape::Slow && pause(ctx, slow).await.is_err() {
                        return StreamOutcome::Abandoned;
                    }
                }
            }
        }
    }

    /// A balanced call over the installed set; returns whether it replied.
    async fn balanced_call(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        permission: Permission,
    ) -> bool {
        if self.set_version == 0
            || runtimes.balanced.status().stale
            || ctx.random().random_bool(0.2)
        {
            self.refresh(ctx, runtimes).await;
        }
        let id = self.id();
        let request = Job {
            id,
            server: String::new(),
            boot: 0,
            configuration: 0,
            hold_ms: 0,
        };
        let policy = permission.policy();
        let observer = &self.observer;
        let primaries_before = Observer::load(&observer.primaries);
        let copies_before = Observer::load(&observer.copies);
        let client = if permission == Permission::Credentialed {
            &runtimes.credentialed
        } else {
            &runtimes.balanced
        };
        let outcome = if permission == Permission::Cancel {
            let give_up = Duration::from_millis(ctx.random().random_range(1..400));
            moonpool_sim::select! {
                outcome = client.call(request, &policy) => Some(outcome),
                _ = pause(ctx, give_up) => None,
            }
        } else {
            Some(client.call(request, &policy).await)
        };
        if permission == Permission::Credentialed && matches!(outcome, Some(Ok(_))) {
            assert_sometimes!(true, "rpc qual credentialed balanced call served");
        }
        let primaries = Observer::load(&observer.primaries) - primaries_before;
        let copies = Observer::load(&observer.copies) - copies_before;
        let sequential = match policy.retry {
            Retry::AtMostOnce => primaries.min(1),
            Retry::AfterAmbiguous => primaries,
        };
        let check = BalancedCheck {
            id,
            policy_cap: policy_cap(&policy),
            permitted: sequential + copies,
            not_admitted: matches!(
                &outcome,
                Some(Err(error)) if error.execution() == Execution::NotAdmitted
            ),
            instances: self.set_instances.clone(),
        };
        check.verify(&QualLedger::of(ctx.state()));
        self.balanced.push(check);
        let summary = match &outcome {
            None => "cancelled".to_string(),
            Some(Ok(done)) => self.judge_balanced(done),
            Some(Err(error)) => self.judge_balance_error(ctx, runtimes, error).await,
        };
        let line = format!("{id} Balanced {permission:?} {summary}");
        tracing::info!(line = %line, "rpc_qual_event");
        self.history.push(line);
        matches!(outcome, Some(Ok(_)))
    }

    fn judge_balanced(&self, done: &Balanced<Done>) -> String {
        let reply = &done.reply;
        assert_always!(
            self.set_instances
                .iter()
                .any(|instance| instance.process == reply.server && instance.boot == reply.boot),
            "rpc qual a balanced reply came from an incarnation of its set"
        );
        let failed_first = done.attempts.iter().any(|attempt| {
            matches!(&attempt.outcome, AttemptOutcome::Failed(_))
                && attempt.alternative != done.alternative
        });
        if failed_first {
            assert_sometimes!(
                true,
                "rpc qual balanced call fell back to another alternative"
            );
        }
        if done.attempts.iter().any(|attempt| {
            matches!(&attempt.outcome, AttemptOutcome::Failed(error) if *error.reason() == ErrorReason::StaleIncarnation)
        }) {
            assert_sometimes!(true, "rpc qual stale incarnation skipped for a healthy one");
        }
        if done
            .attempts
            .iter()
            .any(|attempt| attempt.kind == AttemptKind::Hedge)
        {
            assert_sometimes!(true, "rpc qual balanced call hedged");
        }
        format!("ok {}#{} v{}", reply.server, reply.boot, done.version.get())
    }

    async fn judge_balance_error(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        error: &BalanceError,
    ) -> String {
        if matches!(error.failure(), BalanceFailure::StaleAlternatives) {
            assert_sometimes!(true, "rpc qual balanced set went stale and was replaced");
            for server in ctx.topology().ips_in_group("server") {
                self.forget(&server);
            }
            self.refresh(ctx, runtimes).await;
        }
        let reason = match error.failure() {
            BalanceFailure::Rejected(rejected) => format!("rejected {:?}", rejected.reason()),
            other => format!("{other:?}"),
        };
        format!("err {reason} {:?}", error.execution())
    }

    /// A credentialed call to a server's private endpoint, judged by the
    /// security campaign's oracle; returns whether it replied.
    pub(crate) async fn auth(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        server: Option<String>,
        kind: Kind,
    ) -> bool {
        let Some(server) = server.or_else(|| Self::random_server(ctx)) else {
            return false;
        };
        let Some(publication) = self.publication(ctx, &runtimes.rpc, &server).await else {
            return false;
        };
        let Ok(target) = ServiceRef::<PrivateEcho>::from_bytes(&publication.private) else {
            return false;
        };
        let caller = &runtimes.auth;
        let (id, _) = caller.issue(
            kind,
            Target::Private,
            ctx.random().random_range(1..120),
            ctx.random().random_range(1..60),
        );
        // A short-lived token is presented once the scripted UTC moved past
        // its expiry (or after a while, if the faults are over).
        if kind == Kind::ShortLived {
            let expires = caller
                .ledger
                .issued(id)
                .map_or(0, |issued| issued.minted.expires);
            for _ in 0..40 {
                if caller.trust.utc() >= expires {
                    break;
                }
                if pause(ctx, Duration::from_millis(100)).await.is_err() {
                    return false;
                }
            }
        }
        let outcome = caller.unary(&runtimes.rpc, &target, id, 0).await;
        let _ = caller.judge(id, &outcome.clone().map(Some));
        let minted = caller
            .ledger
            .issued(id)
            .map_or(kind, |issued| issued.minted.kind);
        match &outcome {
            Ok(_) => {
                if minted == Kind::Valid && self.retired_refused {
                    assert_sometimes!(true, "rpc qual rotated key denied then new key accepted");
                    self.retired_refused = false;
                }
            }
            Err(error) => {
                use moonpool_rpc::security::CredentialError;
                match error.reason() {
                    ErrorReason::Unauthenticated(CredentialError::Expired) => {
                        assert_sometimes!(
                            true,
                            "rpc qual expired credential denied before dispatch"
                        );
                    }
                    ErrorReason::Unauthenticated(CredentialError::UnknownKey)
                        if minted == Kind::RotatedOut =>
                    {
                        self.retired_refused = true;
                    }
                    ErrorReason::Unauthenticated(CredentialError::Missing) => {
                        assert_sometimes!(
                            true,
                            "rpc qual private endpoint refused an unauthenticated call"
                        );
                    }
                    _ if is_stale(error) => self.forget(&server),
                    _ => {}
                }
            }
        }
        let line = format!(
            "auth {id} {minted:?} {}#{} {}",
            publication.server,
            publication.boot,
            describe(&outcome)
        );
        tracing::info!(line = %line, "rpc_qual_event");
        self.history.push(line);
        outcome.is_ok()
    }

    /// A call to a stored interface of an ended boot.
    async fn stale(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        let ledger = QualLedger::of(ctx.state());
        let servers = ctx.topology().ips_in_group("server");
        let candidates: Vec<Publication> = self
            .stored
            .iter()
            .filter(|publication| {
                servers.contains(&publication.server)
                    && publication.boot < ledger.current_boot(&publication.server)
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            return;
        }
        let publication = candidates[ctx.random().random_range(0..candidates.len())].clone();
        let Ok(target) = ServiceRef::<Work>::from_bytes(&publication.work) else {
            assert_always!(false, "a stored publication decodes without a runtime");
            return;
        };
        let expected = instance_of(&publication);
        let id = self.id();
        let reliable = ctx.random().random_bool(0.5);
        let client = target.bind(&runtimes.rpc);
        let request = job(id, &expected, 0);
        let outcome = if reliable {
            moonpool_sim::select! {
                outcome = client.get_reply(&request) => Some(outcome),
                _ = pause(ctx, Duration::from_secs(10)) => None,
            }
        } else {
            Some(client.try_get_reply_within(&request, CALL_TIMEOUT).await)
        };
        let Some(outcome) = outcome else {
            self.record_class(
                id,
                QualOp::Stale,
                &expected,
                false,
                Class::Maybe,
                "abandoned",
            );
            return;
        };
        assert_always!(
            outcome.is_err(),
            "rpc qual a stored interface of an ended boot never answers"
        );
        if let Err(error) = &outcome
            && is_stale(error)
        {
            assert_sometimes!(true, "rpc qual stored interface of an ended boot refused");
            if reliable {
                assert_sometimes!(
                    true,
                    "rpc qual retained reliable call to I1 ended terminally"
                );
            }
        }
        self.record(id, QualOp::Stale, &expected, !reliable, &outcome);
    }

    /// Recruit members for a fresh configuration on two servers and hand
    /// them to the third participant; returns whether every member
    /// answered it.
    pub(crate) async fn recruit(&mut self, ctx: &SimContext, runtimes: &Runtimes) -> bool {
        let configuration = self.configuration();
        let servers = ctx.topology().ips_in_group("server");
        let Some(third) = ctx.topology().ips_in_group("third").into_iter().next() else {
            return false;
        };
        let mut members = Vec::new();
        for server in servers.iter().take(2) {
            let Ok(address) = format!("{server}:{RPC_PORT}").parse() else {
                continue;
            };
            let recruiter = WellKnownRef::<Recruiter>::new(
                BootstrapAddress::Resolved(address),
                RECRUITER_ID,
                AccessClass::Public,
            )
            .at(address)
            .bind(&runtimes.rpc);
            match recruiter
                .try_get_reply_within(&Recruit { configuration }, Duration::from_millis(1500))
                .await
            {
                Ok(publication) => {
                    assert_always!(
                        publication.configuration == configuration,
                        "a recruited member names its configuration"
                    );
                    self.learn(ctx, &publication);
                    members.push(publication);
                }
                Err(error) => self
                    .history
                    .push(format!("recruit {server} {}", describe::<()>(&Err(error)))),
            }
        }
        if members.is_empty() {
            return false;
        }
        let ids: Vec<u64> = members.iter().map(|_| self.id()).collect();
        let Ok(address) = format!("{third}:{RPC_PORT}").parse() else {
            return false;
        };
        let adopter = WellKnownRef::<Adopter>::new(
            BootstrapAddress::Resolved(address),
            ADOPTER_ID,
            AccessClass::Public,
        )
        .at(address)
        .bind(&runtimes.rpc);
        let outcome = adopter
            .try_get_reply_within(
                &Adopt {
                    members: members.clone(),
                    ids: ids.clone(),
                },
                CALL_TIMEOUT * 3,
            )
            .await;
        self.judge_relayed(ctx, QualOp::Recruit, &members, &ids, &outcome)
    }

    /// Record every call a relay made for us; returns whether all replied.
    fn judge_relayed(
        &mut self,
        ctx: &SimContext,
        op: QualOp,
        members: &[Publication],
        ids: &[u64],
        outcome: &Result<Relayed, RpcError>,
    ) -> bool {
        let mut all = true;
        for (member, id) in members.iter().zip(ids) {
            let expected = instance_of(member);
            let answer = outcome
                .as_ref()
                .ok()
                .and_then(|relayed| relayed.answers.iter().find(|done| done.id == *id));
            let failure = outcome.as_ref().ok().and_then(|relayed| {
                relayed
                    .failures
                    .iter()
                    .find(|failure| failure.starts_with(&format!("{id}:")))
            });
            let class = match (answer, failure) {
                (Some(done), _) => {
                    Self::judge_reply(ctx, *id, &expected, done);
                    match op {
                        QualOp::Recruit => assert_sometimes!(
                            true,
                            "rpc qual recruited interface invoked by the third participant"
                        ),
                        _ => assert_sometimes!(
                            true,
                            "rpc qual callback invoked through a third participant"
                        ),
                    }
                    Class::Replied
                }
                (None, Some(failure)) => {
                    let class = relayed_class(failure);
                    let stale = failure.contains("StaleIncarnation")
                        || failure.contains("EndpointNotFound");
                    if op == QualOp::Callback
                        && stale
                        && QualLedger::of(ctx.state()).current_boot(&expected.process)
                            > expected.boot
                    {
                        assert_sometimes!(true, "rpc qual stale callback refused");
                    }
                    if stale {
                        self.forget(&expected.process);
                    }
                    class
                }
                // The relay itself failed: nothing is known of the member.
                (None, None) => Class::Maybe,
            };
            all &= class == Class::Replied;
            let note = failure.cloned().unwrap_or_else(|| format!("{class:?}"));
            self.record_class(*id, op, &expected, true, class, &note);
        }
        if let Err(error) = outcome {
            self.history.push(format!(
                "{op:?} relay {}",
                describe::<()>(&Err(error.clone()))
            ));
        }
        all
    }

    /// Hand a server the third participant's callback (a stored one of an
    /// ended boot when `stale`); returns whether it was answered.
    pub(crate) async fn callback(
        &mut self,
        ctx: &SimContext,
        runtimes: &Runtimes,
        server: Option<String>,
        stale: bool,
    ) -> bool {
        let Some(third) = ctx.topology().ips_in_group("third").into_iter().next() else {
            return false;
        };
        let ledger = QualLedger::of(ctx.state());
        let callback = if stale {
            self.stored
                .iter()
                .rfind(|publication| {
                    publication.server == third && publication.boot < ledger.current_boot(&third)
                })
                .cloned()
        } else {
            self.publication(ctx, &runtimes.rpc, &third).await
        };
        let Some(callback) = callback else {
            return false;
        };
        let Some(server) = server.or_else(|| Self::random_server(ctx)) else {
            return false;
        };
        let Ok(address) = format!("{server}:{RPC_PORT}").parse() else {
            return false;
        };
        let forwarder = WellKnownRef::<Forwarder>::new(
            BootstrapAddress::Resolved(address),
            FORWARDER_ID,
            AccessClass::Public,
        )
        .at(address)
        .bind(&runtimes.rpc);
        let id = self.id();
        let outcome = forwarder
            .try_get_reply_within(
                &Forward {
                    callback: Some(callback.clone()),
                    id,
                },
                CALL_TIMEOUT * 2,
            )
            .await;
        self.judge_relayed(ctx, QualOp::Callback, &[callback], &[id], &outcome)
    }

    /// A call to the version 1 legacy peer through the current runtime;
    /// returns whether it replied.
    pub(crate) async fn legacy(&mut self, ctx: &SimContext, runtimes: &Runtimes) -> bool {
        let Some(legacy) = ctx.topology().ips_in_group("legacy").into_iter().next() else {
            return false;
        };
        let Some((expected, target)) = self
            .work_target(ctx, &runtimes.rpc, Some(legacy.clone()))
            .await
        else {
            return false;
        };
        let id = self.id();
        let outcome = target
            .bind(&runtimes.rpc)
            .try_get_reply_within(&job(id, &expected, 0), CALL_TIMEOUT)
            .await;
        match &outcome {
            Ok(done) => {
                Self::judge_reply(ctx, id, &expected, done);
                assert_sometimes!(true, "rpc qual adjacent-version peer interoperated");
            }
            Err(error) => self.judge_error(ctx, id, &expected, error, true),
        }
        self.record(id, QualOp::Legacy, &expected, true, &outcome) == Class::Replied
    }

    /// The version 1 only runtime calls a verifying server (version 2
    /// only): refused at the handshake, never admitted.
    async fn version1(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        let Some((expected, target)) = self.work_target(ctx, &runtimes.rpc, None).await else {
            return;
        };
        let id = self.id();
        let outcome = target
            .bind(&runtimes.version1)
            .try_get_reply_within(&job(id, &expected, 0), CALL_TIMEOUT)
            .await;
        assert_always!(
            outcome.is_err(),
            "rpc qual a v1 client never gets through a verifying server"
        );
        if let Err(error) = &outcome
            && matches!(error.reason(), ErrorReason::ConnectFailed(_))
        {
            assert_always!(
                error.execution() == Execution::NotAdmitted,
                "rpc qual a refused handshake admits nothing"
            );
            assert_sometimes!(true, "rpc qual unsupported version refused observably");
        }
        self.record(id, QualOp::Version1, &expected, true, &outcome);
    }

    /// Many held calls at once to one server (some refused before
    /// admission), then one more that must be served once they drained.
    async fn burst(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        let Some((expected, target)) = self.work_target(ctx, &runtimes.rpc, None).await else {
            return;
        };
        let count = ctx.random().random_range(16..40u32);
        let calls: Vec<(u64, u32)> = (0..count)
            .map(|_| (self.id(), ctx.random().random_range(200..700)))
            .collect();
        let client = target.bind(&runtimes.rpc);
        let outcomes = futures::future::join_all(calls.iter().map(|(id, hold)| {
            let request = job(*id, &expected, *hold);
            let client = &client;
            async move { client.try_get_reply_within(&request, CALL_TIMEOUT).await }
        }))
        .await;
        let mut refused = false;
        for ((id, _), outcome) in calls.into_iter().zip(outcomes) {
            match &outcome {
                Ok(done) => Self::judge_reply(ctx, id, &expected, done),
                Err(error) => {
                    refused |= *error.reason() == ErrorReason::Overloaded;
                    self.judge_error(ctx, id, &expected, error, true);
                }
            }
            self.record(id, QualOp::Burst, &expected, true, &outcome);
        }
        let id = self.id();
        let after = client
            .try_get_reply_within(&job(id, &expected, 0), CALL_TIMEOUT)
            .await;
        if refused && after.is_ok() {
            assert_sometimes!(true, "rpc qual service recovered after an overload burst");
        }
        if let Err(error) = &after {
            self.judge_error(ctx, id, &expected, error, true);
        }
        self.record(id, QualOp::Burst, &expected, true, &after);
    }

    /// A raw session sends a frame header claiming 4 GiB to a server; the
    /// server must close it, and our own session there keep working.
    async fn malformed(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        let Some(server) = Self::random_server(ctx) else {
            return;
        };
        // The server's own count of sessions it closed for a protocol
        // violation, before and after: the refusal must be its own, not a
        // disconnect the chaos caused.
        let violations = |ctx: &SimContext| {
            let label = format!(
                "server@{server}#{:04}",
                QualLedger::of(ctx.state()).current_boot(&server)
            );
            crate::foundations::state::Board::of(ctx.state())
                .stats_with_prefix(&label)
                .first()
                .map(|(label, stats)| (label.clone(), stats.protocol_violations))
        };
        let before = violations(ctx);
        let address = format!("{server}:{RPC_PORT}");
        let connect = ctx.network().connect(&address);
        let Ok(Ok(mut stream)) = ctx.time().timeout(Duration::from_secs(1), connect).await else {
            self.history.push(format!("malformed {server} unreachable"));
            return;
        };
        let refused = async {
            let mut garbage = u32::MAX.to_le_bytes().to_vec();
            garbage.extend_from_slice(&[0xa5; 12]);
            if stream.write_all(&garbage).await.is_err() {
                return true;
            }
            let _ = stream.flush().await;
            let mut sink = [0u8; 256];
            loop {
                match stream.read(&mut sink).await {
                    Ok(0) | Err(_) => return true,
                    Ok(_) => {}
                }
            }
        };
        let closed = ctx
            .time()
            .timeout(Duration::from_secs(3), refused)
            .await
            .unwrap_or(false);
        let answered = self.unary(ctx, runtimes, Some(server.clone())).await;
        // Stats are reported every 100 ms.
        let _ = pause(ctx, Duration::from_millis(250)).await;
        let counted = match (before, violations(ctx)) {
            (Some((was, earlier)), Some((now, later))) => was == now && later > earlier,
            _ => false,
        };
        if closed && answered && counted {
            assert_sometimes!(
                true,
                "rpc qual malformed peer closed, other sessions unaffected"
            );
        }
        self.history.push(format!(
            "malformed {server} closed={closed} answered={answered} counted={counted}"
        ));
    }

    /// Held calls and an endless stream to one server, then a graceful
    /// shutdown of that server under them: calls shorter than the grace
    /// drain, longer ones end at the deadline, the stream fails with the
    /// producer's shutdown code, new calls are refused.
    async fn shutdown_under_work(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        if ctx.state().contains(super::state::SCRIPT_DONE_KEY) {
            return;
        }
        let Some((expected, target)) = self.work_target(ctx, &runtimes.rpc, None).await else {
            return;
        };
        let Some(publication) = self.known.get(&expected.process).cloned() else {
            return;
        };
        let mut asked = ctx
            .state()
            .get::<Vec<String>>(super::state::SHUTDOWN_REQUESTS_KEY)
            .unwrap_or_default();
        asked.push(expected.process.clone());
        ctx.state()
            .publish(super::state::SHUTDOWN_REQUESTS_KEY, asked);
        let calls: Vec<(u64, u32)> = (0..ctx.random().random_range(2..6u32))
            .map(|_| (self.id(), ctx.random().random_range(100..2000)))
            .collect();
        let client = target.bind(&runtimes.rpc);
        let held = futures::future::join_all(calls.iter().map(|(id, hold)| {
            let request = job(*id, &expected, *hold);
            let client = &client;
            async move { client.try_get_reply_within(&request, CALL_TIMEOUT).await }
        }));
        let id = self.id();
        let scan = Scan {
            id,
            count: 0,
            item_bytes: 64,
            first_delay_ms: 0,
            delay_ms: 50,
            end: End::Endless as u32,
            code: 0,
        };
        let stream = async {
            let Ok(target) = ServiceRef::<ScanItems>::from_bytes(&publication.scan) else {
                return None;
            };
            let Ok(stream) = target.bind(&runtimes.rpc).get_reply_stream(&scan) else {
                return None;
            };
            // Bounded: if the shutdown never comes, the caller gives up.
            let consumed = ctx
                .time()
                .timeout(
                    Duration::from_secs(8),
                    self.consume(ctx, stream, id, StreamShape::Complete),
                )
                .await;
            Some(consumed.unwrap_or(StreamOutcome::Abandoned))
        };
        let (outcomes, streamed) = futures::join!(held, stream);
        for ((id, _), outcome) in calls.into_iter().zip(outcomes) {
            match &outcome {
                Ok(done) => Self::judge_reply(ctx, id, &expected, done),
                Err(error) => self.judge_error(ctx, id, &expected, error, true),
            }
            self.record(id, QualOp::Shutdown, &expected, true, &outcome);
        }
        if let Some(outcome) = streamed {
            if let StreamOutcome::Failed(error) = &outcome
                && matches!(error.reason(), ErrorReason::StreamFailed { code } if *code == SHUTDOWN_CODE)
            {
                assert_sometimes!(true, "rpc qual stream failed by a graceful shutdown");
            }
            let taken = crate::streams::state::StreamLedger::of(ctx.state())
                .consumed(id)
                .items
                .len();
            self.history.push(format!(
                "{id} Shutdown stream {}#{} {taken} {}",
                publication.server,
                publication.boot,
                match &outcome {
                    StreamOutcome::Failed(error) =>
                        format!("failed {:?}/{:?}", error.reason(), error.execution()),
                    other => format!("{other:?}"),
                }
            ));
            self.streams.push(StreamCheck {
                id,
                shape: StreamShape::Endless,
                expected: instance_code(&publication.server, publication.boot),
                outcome,
            });
        }
    }
}

/// A stream request of `shape`, with the window its consumer grants.
pub(crate) struct Planned {
    pub(crate) scan: Scan,
    pub(crate) window: u64,
}

impl QualificationWorkload {
    fn plan_stream(ctx: &SimContext, id: u64, shape: StreamShape) -> Planned {
        let item_bytes = ctx.random().random_range(32..512u32);
        let item = stream_item_frame_len(item_bytes as usize + 40);
        let (count, delay, first_delay, window, end) = match shape {
            StreamShape::Eager => {
                let end = [End::Finish, End::Finish, End::Fail, End::Drop]
                    [ctx.random().random_range(0..4)];
                (ctx.random().random_range(2..10u64), 0, 0, item * 16, end)
            }
            StreamShape::Slow => (
                ctx.random().random_range(6..14u64),
                0,
                0,
                item * 2,
                End::Finish,
            ),
            StreamShape::AbandonBefore => (
                4,
                0,
                ctx.random().random_range(300..900u32),
                item * 4,
                End::Finish,
            ),
            StreamShape::AbandonAfter => (20, 30, 0, item * 4, End::Finish),
            StreamShape::Endless => (0, 20, 0, item * 4, End::Endless),
            StreamShape::Complete => (4, 0, 0, item * 8, End::Finish),
        };
        let scan = Scan {
            id,
            count,
            item_bytes,
            first_delay_ms: first_delay,
            delay_ms: delay,
            end: end as u32,
            code: 7,
        };
        Planned { scan, window }
    }
}
