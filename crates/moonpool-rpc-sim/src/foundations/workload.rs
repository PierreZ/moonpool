//! The surviving workload: drives the operation mix, keeps its own history
//! and judges it against the receipt ledger.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use futures::{AsyncReadExt, AsyncWriteExt};
use moonpool_rpc::protocol::{PROTOCOL_MAGIC, WireMessage, encode_frame, encode_message};
use moonpool_rpc::{
    AccessClass, ErrorReason, Execution, Incarnation, RpcDriver, RpcError, RpcHandle, ServiceRef,
    Wire,
};
use moonpool_sim::{
    NetworkProvider, RandomProvider, SimContext, SimProviders, SimulationResult, TimeProvider,
    Workload, assert_always, assert_sometimes,
};

use super::messages::{
    CrashAfterReceipt, Echo, EchoForeignCodec, EchoNextSchema, Echoed, Ephemeral, ForeignBody,
    Probe, Relay, RelayOutcome, Slow, WrongMethod,
};
use super::state::{Board, EPHEMERAL_KEY, Ledger, RELAY_REF_KEY, SERVER_REFS_KEY, ServerRefs};
use super::{CALL_TIMEOUT, Observations, RPC_PORT, RunRecord, rpc_config};

/// One operation of the workload's alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// Direct typed call to the server's echo.
    Echo,
    /// Call through the relay process.
    Relay,
    /// Call the slow endpoint with a deadline shorter than its work.
    SlowCancel,
    /// Call the latest or an already destroyed ephemeral endpoint.
    Ephemeral,
    /// Call echo's endpoint with a forged method, schema or codec.
    Mismatch,
    /// Open a raw session and send a corrupt, oversized, garbled or
    /// unsupported frame.
    Malformed,
    /// Call the crash-after-receipt endpoint (the fault script crashes the
    /// server once the handler has it).
    Crash,
    /// Call a reference from an earlier server incarnation.
    Stale,
}

impl Op {
    const ALL: [Self; 8] = [
        Self::Echo,
        Self::Relay,
        Self::SlowCancel,
        Self::Ephemeral,
        Self::Mismatch,
        Self::Malformed,
        Self::Crash,
        Self::Stale,
    ];
}

/// The workload's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadConfig {
    /// Operations per run.
    pub operations: usize,
    /// Relative weight per [`Op`], in `Op::ALL` order; zero disables one.
    pub weights: [u32; 8],
    /// Pause between operations, in milliseconds (half-open range).
    pub gap_ms: (u64, u64),
}

impl WorkloadConfig {
    /// The full campaign mix.
    #[must_use]
    pub fn campaign() -> Self {
        Self {
            operations: 60,
            weights: [30, 20, 8, 10, 8, 6, 2, 6],
            gap_ms: (20, 250),
        }
    }

    /// Heavy plain traffic: many small calls, for the bit-flip scenario.
    #[must_use]
    pub fn traffic() -> Self {
        Self {
            operations: 400,
            weights: [3, 1, 0, 0, 0, 0, 0, 0],
            gap_ms: (1, 10),
        }
    }
}

/// What an outcome proves, as the ledger oracle judges it.
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

/// The campaign's workload.
pub struct FoundationsWorkload {
    config: WorkloadConfig,
    observations: Observations,
    history: Vec<String>,
    calls: Vec<(u64, Op, Class)>,
    next_id: u64,
    /// Ephemeral references known destroyed (they served their one call).
    destroyed: BTreeSet<Vec<u8>>,
    /// Every server publication seen, oldest first.
    publications: Vec<ServerRefs>,
}

impl FoundationsWorkload {
    /// A fresh workload appending its run record to `observations`.
    #[must_use]
    pub fn new(config: WorkloadConfig, observations: Observations) -> Self {
        Self {
            config,
            observations,
            history: Vec::new(),
            calls: Vec::new(),
            next_id: 0,
            destroyed: BTreeSet::new(),
            publications: Vec::new(),
        }
    }

    fn record(&mut self, id: u64, op: Op, class: Class, detail: &str) {
        self.calls.push((id, op, class));
        self.history.push(format!("{id} {op:?} {class:?} {detail}"));
    }

    fn pick(&self, ctx: &SimContext) -> Op {
        let total: u32 = self.config.weights.iter().sum();
        let mut draw = ctx.random().random_range(0..total.max(1));
        for (op, weight) in Op::ALL.into_iter().zip(self.config.weights) {
            if draw < weight {
                return op;
            }
            draw -= weight;
        }
        Op::Echo
    }

    fn refresh_publications(&mut self, ctx: &SimContext) -> Option<ServerRefs> {
        let latest = ctx.state().get::<ServerRefs>(SERVER_REFS_KEY)?;
        if self.publications.last() != Some(&latest) {
            self.publications.push(latest.clone());
        }
        Some(latest)
    }

    async fn drive(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
    ) -> SimulationResult<()> {
        // Wait for both processes to publish their references.
        while self.refresh_publications(ctx).is_none() || !ctx.state().contains(RELAY_REF_KEY) {
            if ctx.shutdown().is_cancelled()
                || ctx.time().sleep(Duration::from_millis(10)).await.is_err()
            {
                return Ok(());
            }
        }
        for _ in 0..self.config.operations {
            if ctx.shutdown().is_cancelled() {
                break;
            }
            let op = self.pick(ctx);
            self.step(op, ctx, rpc).await;
            let (low, high) = self.config.gap_ms;
            let gap = ctx.random().random_range(low..high.max(low + 1));
            if ctx.time().sleep(Duration::from_millis(gap)).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn step(&mut self, op: Op, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        let Some(latest) = self.refresh_publications(ctx) else {
            return;
        };
        self.next_id += 1;
        let id = self.next_id;
        match op {
            Op::Echo => self.echo(id, &latest.echo, rpc, Op::Echo).await,
            Op::Stale => {
                // The oldest reference of an earlier incarnation, if any.
                let stale = self
                    .publications
                    .iter()
                    .find(|refs| refs.boot < latest.boot)
                    .map(|refs| refs.echo.clone());
                match stale {
                    Some(bytes) => self.echo(id, &bytes, rpc, Op::Stale).await,
                    None => self.echo(id, &latest.echo, rpc, Op::Echo).await,
                }
            }
            Op::Relay => self.relay(id, ctx, rpc).await,
            Op::SlowCancel => self.slow(id, &latest.slow, ctx, rpc).await,
            Op::Ephemeral => self.ephemeral(id, ctx, rpc).await,
            Op::Mismatch => self.mismatch(id, &latest.echo, ctx, rpc).await,
            Op::Malformed => self.malformed(id, ctx).await,
            Op::Crash => self.crash(id, &latest.crash, rpc).await,
        }
    }

    async fn echo(&mut self, id: u64, bytes: &[u8], rpc: &RpcHandle<SimProviders>, op: Op) {
        let Ok(echo) = ServiceRef::<Echo>::from_bytes(bytes) else {
            return;
        };
        let outcome = echo
            .bind(rpc)
            .try_get_reply_within(&Probe::new(id), CALL_TIMEOUT)
            .await;
        if let Ok(reply) = &outcome {
            assert_always!(
                reply.id == id && reply.text == Probe::new(id).text,
                "a reply matches its request"
            );
            assert_sometimes!(true, "rpc typed call succeeded");
        }
        if op == Op::Stale {
            assert_always!(
                outcome.is_err(),
                "a reference from an earlier incarnation never reaches the new one"
            );
            if matches!(&outcome, Err(error) if *error.reason() == ErrorReason::StaleIncarnation) {
                assert_sometimes!(true, "rpc stale incarnation rejected after restart");
            }
        }
        if let Err(error) = &outcome
            && *error.reason() == ErrorReason::Disconnected
        {
            assert_sometimes!(true, "rpc call failed by disconnect");
        }
        self.record(id, op, classify(&outcome), &describe(&outcome));
    }

    async fn relay(&mut self, id: u64, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        let Some(relay) = ctx
            .state()
            .get::<Vec<u8>>(RELAY_REF_KEY)
            .and_then(|bytes| ServiceRef::<Relay>::from_bytes(&bytes).ok())
        else {
            return;
        };
        let outcome = relay
            .bind(rpc)
            .try_get_reply_within(&Probe::new(id), CALL_TIMEOUT)
            .await;
        let class = match &outcome {
            Ok(relayed) => match RelayOutcome::from_u32(relayed.outcome) {
                Some(RelayOutcome::Replied) => {
                    assert_always!(
                        relayed.id == id && relayed.text == Probe::new(id).text,
                        "a relayed reply matches its request"
                    );
                    assert_sometimes!(true, "rpc process-to-process call succeeded");
                    Class::Replied
                }
                Some(RelayOutcome::NotAdmitted) => Class::NotAdmitted,
                Some(RelayOutcome::Executed) => Class::Executed,
                _ => Class::Maybe,
            },
            // The relay may have forwarded before its own reply was lost.
            Err(error) if error.execution() == Execution::NotAdmitted => Class::NotAdmitted,
            Err(_) => Class::Maybe,
        };
        self.record(id, Op::Relay, class, &describe(&outcome));
    }

    async fn slow(
        &mut self,
        id: u64,
        bytes: &[u8],
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
    ) {
        let Ok(slow) = ServiceRef::<Slow>::from_bytes(bytes) else {
            return;
        };
        let deadline = Duration::from_millis(ctx.random().random_range(10..150));
        let outcome = slow
            .bind(rpc)
            .try_get_reply_within(&Probe::new(id), deadline)
            .await;
        if let Err(error) = &outcome
            && *error.reason() == ErrorReason::Timeout
        {
            assert_sometimes!(
                error.execution() == Execution::MaybeExecuted,
                "rpc caller gave up after the request left"
            );
        }
        self.record(id, Op::SlowCancel, classify(&outcome), &describe(&outcome));
    }

    async fn ephemeral(&mut self, id: u64, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        let Some((_, latest)) = ctx.state().get::<(u64, Vec<u8>)>(EPHEMERAL_KEY) else {
            return;
        };
        let target = if !self.destroyed.is_empty() && ctx.random().random_bool(0.5) {
            let index = ctx.random().random_range(0..self.destroyed.len());
            self.destroyed.iter().nth(index).cloned().unwrap_or(latest)
        } else {
            latest
        };
        let Ok(ephemeral) = ServiceRef::<Ephemeral>::from_bytes(&target) else {
            return;
        };
        let known_destroyed = self.destroyed.contains(&target);
        let outcome = ephemeral
            .bind(rpc)
            .try_get_reply_within(&Probe::new(id), CALL_TIMEOUT)
            .await;
        if known_destroyed {
            assert_always!(outcome.is_err(), "a destroyed endpoint never serves again");
            if matches!(&outcome, Err(error) if *error.reason() == ErrorReason::EndpointNotFound) {
                assert_sometimes!(true, "rpc destroyed endpoint rejected a later call");
            }
        }
        if outcome.is_ok() {
            self.destroyed.insert(target);
        }
        self.record(id, Op::Ephemeral, classify(&outcome), &describe(&outcome));
    }

    async fn mismatch(
        &mut self,
        id: u64,
        bytes: &[u8],
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
    ) {
        let Ok(echo) = ServiceRef::<Echo>::from_bytes(bytes) else {
            return;
        };
        let endpoint = *echo.endpoint();
        let outcome: Result<Echoed, RpcError> = match ctx.random().random_range(0..3) {
            0 => {
                ServiceRef::<EchoNextSchema>::new(endpoint, AccessClass::Public)
                    .bind(rpc)
                    .try_get_reply_within(&Probe::new(id), CALL_TIMEOUT)
                    .await
            }
            1 => {
                ServiceRef::<WrongMethod>::new(endpoint, AccessClass::Public)
                    .bind(rpc)
                    .try_get_reply_within(&Probe::new(id), CALL_TIMEOUT)
                    .await
            }
            _ => {
                // A well-formed echo body under a foreign codec id: decoding
                // it would succeed, so only the codec check keeps it out.
                let mut body = Vec::new();
                let _ = Probe::new(id).encode(&mut body);
                ServiceRef::<EchoForeignCodec>::new(endpoint, AccessClass::Public)
                    .bind(rpc)
                    .try_get_reply_within(&ForeignBody(body), CALL_TIMEOUT)
                    .await
            }
        };
        assert_always!(
            outcome.is_err(),
            "a mismatched contract never reaches a handler"
        );
        if let Err(error) = &outcome
            && matches!(
                error.reason(),
                ErrorReason::MethodMismatch { .. }
                    | ErrorReason::SchemaMismatch { .. }
                    | ErrorReason::CodecMismatch { .. }
            )
        {
            assert_sometimes!(true, "rpc contract mismatch rejected before decode");
        }
        self.record(id, Op::Mismatch, classify(&outcome), &describe(&outcome));
    }

    async fn crash(&mut self, id: u64, bytes: &[u8], rpc: &RpcHandle<SimProviders>) {
        let Ok(crash) = ServiceRef::<CrashAfterReceipt>::from_bytes(bytes) else {
            return;
        };
        let outcome = crash
            .bind(rpc)
            .try_get_reply_within(&Probe::new(id), CALL_TIMEOUT)
            .await;
        if let Err(error) = &outcome
            && *error.reason() == ErrorReason::Disconnected
            && error.execution() == Execution::MaybeExecuted
        {
            assert_sometimes!(true, "rpc disconnect after server receipt is ambiguous");
        }
        self.record(id, Op::Crash, classify(&outcome), &describe(&outcome));
    }

    async fn malformed(&mut self, id: u64, ctx: &SimContext) {
        let Some(server) = ctx.topology().ips_in_group("server").into_iter().next() else {
            return;
        };
        let (kind, bytes) = malformed_input(ctx.random().random_range(0..4), id);
        let address = format!("{server}:{RPC_PORT}");
        let connect = ctx.network().connect(&address);
        let Ok(Ok(mut stream)) = ctx.time().timeout(Duration::from_secs(1), connect).await else {
            self.history
                .push(format!("{id} Malformed {kind} unreachable"));
            return;
        };
        let closed = async {
            if stream.write_all(&bytes).await.is_err() {
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
            .timeout(Duration::from_secs(3), closed)
            .await
            .unwrap_or(false);
        if closed {
            assert_sometimes!(true, "rpc malformed input closed the session");
        }
        self.history
            .push(format!("{id} Malformed {kind} closed={closed}"));
    }
}

/// One malformed opening for a raw session.
fn malformed_input(kind: u64, id: u64) -> (&'static str, Vec<u8>) {
    let hello = |min_version, max_version| {
        encode_frame(
            &encode_message(&WireMessage::Hello {
                magic: PROTOCOL_MAGIC,
                min_version,
                max_version,
                incarnation: Incarnation::from_raw(u128::from(id)),
                features: 0,
            }),
            1024,
        )
        .unwrap_or_default()
    };
    match kind {
        0 => {
            let mut corrupt = hello(1, 1);
            if let Some(last) = corrupt.last_mut() {
                *last ^= 0x01;
            }
            ("checksum", corrupt)
        }
        1 => {
            let mut oversized = u32::MAX.to_le_bytes().to_vec();
            oversized.extend_from_slice(&[0; 8]);
            ("oversized", oversized)
        }
        2 => {
            let mut garbled = hello(1, 1);
            garbled.extend(encode_frame(&[0x7F, 1, 2, 3], 1024).unwrap_or_default());
            ("envelope", garbled)
        }
        _ => ("version", hello(900, 999)),
    }
}

fn describe<T>(outcome: &Result<T, RpcError>) -> String {
    match outcome {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("{:?}/{:?}", error.reason(), error.execution()),
    }
}

#[async_trait]
impl Workload for FoundationsWorkload {
    fn name(&self) -> &'static str {
        "rpc_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let (driver, rpc) = RpcDriver::client_only(ctx.providers().clone(), rpc_config());
        let probe = rpc.probe();
        if let Some(probe) = &probe {
            Board::of(ctx.state()).register_probe("workload", probe.clone());
        }
        let result = moonpool_sim::select! {
            () = driver.run() => Ok(()),
            result = self.drive(ctx, &rpc) => result,
        };
        // The driver future was dropped with the select: the runtime, its
        // connections and child futures must be gone, whatever was pending.
        if let Some(probe) = &probe {
            assert_always!(
                probe.is_released(),
                "the workload runtime released everything once its driver dropped"
            );
        }
        result
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let ledger = Ledger::of(ctx.state());
        for &(id, op, class) in &self.calls {
            let receipts = ledger.receipts(id);
            assert_always!(receipts <= 1, "no request executes more than once");
            match class {
                Class::Replied | Class::Executed => {
                    assert_always!(
                        receipts == 1,
                        "a call that produced an outcome executed exactly once"
                    );
                }
                Class::NotAdmitted => {
                    assert_always!(
                        receipts == 0,
                        "a call reported not admitted never reached a handler"
                    );
                }
                Class::Maybe => {}
            }
            tracing::debug!(id, ?op, ?class, receipts, "rpc call judged");
        }
        let totals = Board::of(ctx.state()).totals();
        assert_sometimes!(
            totals.checksum_failures > 0,
            "rpc checksum mismatch observed"
        );
        assert_sometimes!(
            totals.version_rejections > 0,
            "rpc unsupported protocol version refused"
        );
        self.observations
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(RunRecord {
                history: std::mem::take(&mut self.history),
                checksum_failures: totals.checksum_failures,
                protocol_violations: totals.protocol_violations,
            });
        Ok(())
    }
}
