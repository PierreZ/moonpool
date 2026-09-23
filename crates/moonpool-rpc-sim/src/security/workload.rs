//! The surviving workload: two client runtimes (one speaking both protocol
//! versions, one pinned to version 1) drive the operation mix, judge each
//! outcome as it comes, and check every issued request against the receipt
//! ledger at the end.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::security::{Credential, CredentialSource};
use moonpool_rpc::{Endpoint, ErrorReason, RpcConfig, RpcDriver, RpcHandle, ServiceRef};
use moonpool_sim::{
    RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult, Workload,
    assert_always, assert_sometimes,
};

use super::judge::{Caller, describe};
use super::messages::{
    Answer, PrivateEcho, PrivateNote, PrivateScan, Probe, PublicEcho, SCAN_ITEMS,
};
use super::state::{
    CUT_REQUESTS_KEY, Class, DRAINING_KEY, LEGACY_REFS_KEY, Ledger, LegacyRefs, Route,
    SCRIPT_DONE_KEY, SERVER_REFS_KEY, ServerRefs, Target, WORKLOAD_IP_KEY,
};
use super::trust::{Kind, Trust};
use super::{CALL_TIMEOUT, SecurityRecord, SecurityRecords, pause};
use crate::foundations::rpc_config;

/// One operation of the workload's alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SecurityOp {
    /// A unary call to the private endpoint with a drawn credential.
    Private,
    /// A unary call to the public endpoint with a drawn credential.
    Public,
    /// A reply stream from the private streaming endpoint.
    Scan,
    /// A one-way request to a private endpoint.
    Note,
    /// The same token used twice, some UTC apart.
    Reuse,
    /// A reliable call whose credential source mints a fresh token for
    /// every attempt, held across disconnects and reboots.
    Reliable,
    /// A call through the version 1 only runtime to the verifying server.
    Version1,
    /// A call to the version 1 legacy server.
    Legacy,
    /// Several held private calls at once: work in flight when a
    /// graceful shutdown or a crash comes.
    Burst,
}

impl SecurityOp {
    const ALL: [Self; 9] = [
        Self::Private,
        Self::Public,
        Self::Scan,
        Self::Note,
        Self::Reuse,
        Self::Reliable,
        Self::Version1,
        Self::Legacy,
        Self::Burst,
    ];
}

/// The workload's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityConfig {
    /// Operations per run.
    pub operations: usize,
    /// Relative weight per [`SecurityOp`], in declaration order.
    pub weights: [u32; 9],
    /// Pause between operations, in milliseconds (half-open range).
    pub gap_ms: (u64, u64),
}

impl SecurityConfig {
    /// The campaign mix.
    #[must_use]
    pub fn campaign() -> Self {
        Self {
            operations: 45,
            weights: [30, 12, 8, 6, 8, 6, 4, 6, 12],
            gap_ms: (10, 200),
        }
    }
}

/// Mints a fresh, valid token (current key, current UTC) for every attempt
/// and counts how often it was asked.
struct Refresher {
    trust: Trust,
    ledger: Ledger,
    id: u64,
    subject: String,
    asks: Arc<AtomicU32>,
}

impl CredentialSource for Refresher {
    fn credential(&self, _target: &Endpoint) -> Option<Credential> {
        self.asks.fetch_add(1, Ordering::Relaxed);
        let minted = self.trust.mint(Kind::Refreshing, &self.subject, 120, 0);
        // Recorded before it can leave: the oracle judges each receipt
        // against the tokens actually minted, at their mint.
        self.ledger.mint(
            self.id,
            minted.clone(),
            self.trust.utc(),
            self.trust.generation(),
        );
        minted.token.map(Credential::bearer)
    }
}

/// While the operations run: whenever a server boot starts draining for a
/// graceful shutdown, send it a call at once, which it must refuse without
/// running it (the drain lasts as long as its admitted work). Returns its
/// history lines.
async fn drain_watcher(
    ctx: &SimContext,
    rpc: &RpcHandle<SimProviders>,
    remote: &Caller,
    done: &std::sync::atomic::AtomicBool,
) -> Vec<String> {
    let mut history = Vec::new();
    let mut probed = 0;
    while !done.load(Ordering::Relaxed) {
        if pause(ctx, Duration::from_millis(20)).await.is_err() {
            break;
        }
        let draining = ctx.state().get::<u64>(DRAINING_KEY).unwrap_or(0);
        let Some(refs) = ctx.state().get::<ServerRefs>(SERVER_REFS_KEY) else {
            continue;
        };
        if draining != refs.boot || draining == probed {
            continue;
        }
        probed = draining;
        let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private) else {
            continue;
        };
        let (id, _) = remote.issue(Kind::Valid, Target::Private, 120, 0);
        let outcome = remote.unary(rpc, &service, id, 0).await;
        let _ = remote.judge(id, &outcome.clone().map(Some));
        history.push(format!("{id} Drain Valid {}", describe(&outcome)));
    }
    history
}

/// The workload runtimes' configuration: the simulation's sessions are
/// plaintext, so they opt into sending credentials over them explicitly.
fn client_config() -> RpcConfig {
    let config = rpc_config();
    RpcConfig {
        security: config.security.clone().send_credentials_over_plaintext(),
        ..config
    }
}

/// The campaign's workload.
pub struct SecurityWorkload {
    config: SecurityConfig,
    records: SecurityRecords,
    history: Vec<String>,
}

struct Runtimes {
    rpc: RpcHandle<SimProviders>,
    version1: RpcHandle<SimProviders>,
}

impl SecurityWorkload {
    /// A fresh workload appending its run record to `records`.
    #[must_use]
    pub fn new(config: SecurityConfig, records: SecurityRecords) -> Self {
        Self {
            config,
            records,
            history: Vec::new(),
        }
    }

    fn pick(&self, ctx: &SimContext) -> SecurityOp {
        let total: u32 = self.config.weights.iter().sum();
        let mut draw = ctx.random().random_range(0..total.max(1));
        for (op, weight) in SecurityOp::ALL.into_iter().zip(self.config.weights) {
            if draw < weight {
                return op;
            }
            draw -= weight;
        }
        SecurityOp::Private
    }

    fn draw_kind(ctx: &SimContext) -> Kind {
        Kind::DRAWN[ctx.random().random_range(0..Kind::DRAWN.len())]
    }

    fn note(&mut self, id: u64, op: SecurityOp, kind: Kind, outcome: &str) {
        self.history.push(format!("{id} {op:?} {kind:?} {outcome}"));
    }

    async fn drive(&mut self, ctx: &SimContext, runtimes: &Runtimes) -> SimulationResult<()> {
        let trust = Trust::of(ctx.state())?;
        let ledger = Ledger::of(ctx.state());
        while !ctx.state().contains(SERVER_REFS_KEY) || !ctx.state().contains(LEGACY_REFS_KEY) {
            if ctx.shutdown().is_cancelled() || pause(ctx, Duration::from_millis(10)).await.is_err()
            {
                return Ok(());
            }
        }
        let remote = Caller {
            ledger: ledger.clone(),
            trust: trust.clone(),
            route: Route::Remote,
        };
        ctx.state()
            .publish(WORKLOAD_IP_KEY, ctx.my_ip().to_string());
        let done = std::sync::atomic::AtomicBool::new(false);
        let watcher = drain_watcher(ctx, &runtimes.rpc, &remote, &done);
        let operations = async {
            for _ in 0..self.config.operations {
                if ctx.shutdown().is_cancelled() {
                    break;
                }
                let op = self.pick(ctx);
                self.step(op, ctx, runtimes, &remote).await;
                let (low, high) = self.config.gap_ms;
                let gap = ctx.random().random_range(low..high.max(low + 1));
                if pause(ctx, Duration::from_millis(gap)).await.is_err() {
                    break;
                }
            }
            done.store(true, Ordering::Relaxed);
        };
        let ((), drained) = futures::join!(operations, watcher);
        self.history.extend(drained);
        if ctx.shutdown().is_cancelled() {
            return Ok(());
        }
        self.recover(ctx, &runtimes.rpc, &remote).await
    }

    fn server_refs(ctx: &SimContext) -> Option<ServerRefs> {
        ctx.state().get::<ServerRefs>(SERVER_REFS_KEY)
    }

    async fn step(
        &mut self,
        op: SecurityOp,
        ctx: &SimContext,
        runtimes: &Runtimes,
        remote: &Caller,
    ) {
        let Some(refs) = Self::server_refs(ctx) else {
            return;
        };
        let ttl = ctx.random().random_range(1..120);
        let delay = ctx.random().random_range(1..60);
        let hold = ctx.random().random_range(0..300u32);
        match op {
            SecurityOp::Private | SecurityOp::Public => {
                let kind = Self::draw_kind(ctx);
                let outcome = if op == SecurityOp::Private {
                    let (id, _) = remote.issue(kind, Target::Private, ttl, delay);
                    let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private) else {
                        return;
                    };
                    (id, remote.unary(&runtimes.rpc, &service, id, hold).await)
                } else {
                    let (id, _) = remote.issue(kind, Target::Public, ttl, delay);
                    let Ok(service) = ServiceRef::<PublicEcho>::from_bytes(&refs.public) else {
                        return;
                    };
                    (id, remote.unary(&runtimes.rpc, &service, id, hold).await)
                };
                let (id, outcome) = outcome;
                let _ = remote.judge(id, &outcome.clone().map(Some));
                self.note(id, op, kind, &describe(&outcome));
            }
            SecurityOp::Scan => self.scan(ctx, &runtimes.rpc, remote, &refs).await,
            SecurityOp::Note => {
                let kind = Self::draw_kind(ctx);
                let (id, minted) = remote.issue(kind, Target::Note, ttl, delay);
                let Ok(service) = ServiceRef::<PrivateNote>::from_bytes(&refs.note) else {
                    return;
                };
                let mut client = service.bind(&runtimes.rpc);
                if let Some(token) = minted.token {
                    client = client.with_credentials(Credential::bearer(token));
                }
                let sent = client.send(&Probe { id, hold_ms: 0 });
                // Queued proves nothing; the handler's oracle judges it.
                remote.ledger.outcome(
                    id,
                    if sent.is_ok() {
                        Class::Maybe
                    } else {
                        Class::NotAdmitted
                    },
                );
                if sent.is_ok() && !kind.is_signed_by_the_issuer() && kind != Kind::Anonymous {
                    assert_sometimes!(true, "rpc security invalid one-way request sent");
                }
                self.note(
                    id,
                    op,
                    kind,
                    if sent.is_ok() { "queued" } else { "refused" },
                );
            }
            SecurityOp::Reuse => self.reuse(ctx, &runtimes.rpc, remote, &refs).await,
            SecurityOp::Reliable => self.reliable(ctx, &runtimes.rpc, remote, &refs).await,
            SecurityOp::Version1 => {
                let version1 = Caller {
                    route: Route::Version1,
                    ..remote.clone()
                };
                let (id, _) = version1.issue(Kind::Valid, Target::Private, 120, 0);
                let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private) else {
                    return;
                };
                let outcome = version1.unary(&runtimes.version1, &service, id, 0).await;
                let _ = version1.judge(id, &outcome.clone().map(Some));
                self.note(id, op, Kind::Valid, &describe(&outcome));
            }
            SecurityOp::Legacy => self.legacy(ctx, &runtimes.rpc, remote).await,
            SecurityOp::Burst => self.burst(ctx, &runtimes.rpc, remote, &refs).await,
        }
    }

    async fn burst(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        remote: &Caller,
        refs: &ServerRefs,
    ) {
        let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private) else {
            return;
        };
        let count = ctx.random().random_range(3..9u32);
        let calls: Vec<(u64, Kind, u32)> = (0..count)
            .map(|_| {
                let kind = if ctx.random().random_bool(0.8) {
                    Kind::Valid
                } else {
                    Self::draw_kind(ctx)
                };
                let (id, _) = remote.issue(kind, Target::Private, 120, 5);
                (id, kind, ctx.random().random_range(200..900))
            })
            .collect();
        let outcomes = futures::future::join_all(calls.iter().map(|(id, _, hold)| {
            let service = service.clone();
            async move { remote.unary(rpc, &service, *id, *hold).await }
        }))
        .await;
        for ((id, kind, _), outcome) in calls.into_iter().zip(outcomes) {
            let _ = remote.judge(id, &outcome.clone().map(Some));
            self.note(id, SecurityOp::Burst, kind, &describe(&outcome));
        }
    }

    async fn scan(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        remote: &Caller,
        refs: &ServerRefs,
    ) {
        let kind = Self::draw_kind(ctx);
        let (id, minted) = remote.issue(
            kind,
            Target::Scan,
            ctx.random().random_range(1..120),
            ctx.random().random_range(1..60),
        );
        let Ok(service) = ServiceRef::<PrivateScan>::from_bytes(&refs.scan) else {
            return;
        };
        let mut client = service.bind(rpc);
        if let Some(token) = minted.token {
            client = client.with_credentials(Credential::bearer(token));
        }
        let mut stream = match client.get_reply_stream(&Probe { id, hold_ms: 0 }) {
            Ok(stream) => stream,
            Err(error) => {
                let outcome: Result<Option<Answer>, _> = Err(error);
                let _ = remote.judge(id, &outcome);
                self.note(id, SecurityOp::Scan, kind, &describe(&outcome));
                return;
            }
        };
        let mut items = 0;
        let mut last: Option<Answer> = None;
        let ending = loop {
            let next = moonpool_sim::select! {
                item = stream.recv() => item,
                _ = pause(ctx, CALL_TIMEOUT) => break None,
            };
            match next {
                Some(Ok(item)) => {
                    items += 1;
                    last = Some(item);
                }
                Some(Err(error)) => break Some(error),
                None => break None,
            }
        };
        let outcome = match (ending, items) {
            (Some(error), 0) => Err(error),
            (None, 0) => {
                // No item and no verdict before the deadline: unknown.
                remote.ledger.outcome(id, Class::Maybe);
                self.note(id, SecurityOp::Scan, kind, "no verdict");
                return;
            }
            _ => Ok(last),
        };
        if items == SCAN_ITEMS {
            assert_sometimes!(true, "rpc security authorized stream delivered its items");
        }
        let class = remote.judge(id, &outcome);
        if class == Class::NotAdmitted
            && matches!(
                outcome.as_ref().map_err(moonpool_rpc::RpcError::reason),
                Err(ErrorReason::Unauthenticated(_))
            )
        {
            assert_sometimes!(
                true,
                "rpc security unauthorized stream refused before any item"
            );
        }
        self.note(
            id,
            SecurityOp::Scan,
            kind,
            &format!("{items} items {}", describe(&outcome)),
        );
    }

    /// One token, two calls some UTC apart: a token that was accepted may
    /// be refused later (expired, or its key rotated out), never the
    /// reverse for a token that was never acceptable.
    async fn reuse(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        remote: &Caller,
        refs: &ServerRefs,
    ) {
        let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private) else {
            return;
        };
        let kind = if ctx.random().random_bool(0.5) {
            Kind::ShortLived
        } else {
            Kind::Valid
        };
        let (first, minted) =
            remote.issue(kind, Target::Private, ctx.random().random_range(1..8), 0);
        let subject = remote
            .ledger
            .issued(first)
            .map(|issued| issued.subject)
            .unwrap_or_default();
        let outcome = remote.unary(rpc, &service, first, 0).await;
        let _ = remote.judge(first, &outcome.clone().map(Some));
        self.note(first, SecurityOp::Reuse, kind, &describe(&outcome));
        if pause(
            ctx,
            Duration::from_millis(ctx.random().random_range(200..2000)),
        )
        .await
        .is_err()
        {
            return;
        }
        let second = remote.reissue(&minted, &subject, Target::Private);
        let again = remote.unary(rpc, &service, second, 0).await;
        let _ = remote.judge(second, &again.clone().map(Some));
        if outcome.is_ok()
            && matches!(
                again.as_ref().map_err(moonpool_rpc::RpcError::reason),
                Err(ErrorReason::Unauthenticated(_))
            )
        {
            assert_sometimes!(true, "rpc security the same token accepted, then refused");
        }
        self.note(second, SecurityOp::Reuse, kind, &describe(&again));
    }

    /// A reliable call: every retransmission asks the source again, so a
    /// reconnect never loses (or freezes) the credential.
    async fn reliable(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        remote: &Caller,
        refs: &ServerRefs,
    ) {
        let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private) else {
            return;
        };
        let (id, minted) = remote.issue(Kind::Refreshing, Target::Private, 120, 0);
        let subject = remote
            .ledger
            .issued(id)
            .map(|issued| issued.subject)
            .unwrap_or_default();
        let asks = Arc::new(AtomicU32::new(0));
        let client = service.bind(rpc).with_credentials(Refresher {
            trust: remote.trust.clone(),
            ledger: remote.ledger.clone(),
            id,
            subject,
            asks: Arc::clone(&asks),
        });
        let probe = Probe {
            id,
            hold_ms: ctx.random().random_range(200..1500),
        };
        // Ask the script to cut this session while the call is held: the
        // call is then sent again, with a fresh credential, on the next one.
        if ctx.random().random_bool(0.5) {
            let asked = ctx.state().get::<u64>(CUT_REQUESTS_KEY).unwrap_or(0);
            ctx.state().publish(CUT_REQUESTS_KEY, asked + 1);
        }
        let outcome = moonpool_sim::select! {
            outcome = client.get_reply(&probe) => Some(outcome),
            _ = pause(ctx, Duration::from_secs(15)) => None,
        };
        let Some(outcome) = outcome else {
            // Abandoned: nothing proven either way.
            remote.ledger.outcome(id, Class::Maybe);
            self.note(id, SecurityOp::Reliable, minted.kind, "abandoned");
            return;
        };
        if let Err(error) = &outcome {
            assert_always!(
                !matches!(
                    error.reason(),
                    ErrorReason::Unauthenticated(moonpool_rpc::security::CredentialError::Missing)
                ),
                "a retransmitted reliable call never loses its credential"
            );
        }
        if outcome.is_ok() && asks.load(Ordering::Relaxed) >= 2 {
            assert_sometimes!(
                true,
                "rpc security reliable call resent with a fresh credential"
            );
        }
        let _ = remote.judge(id, &outcome.clone().map(Some));
        self.note(id, SecurityOp::Reliable, minted.kind, &describe(&outcome));
    }

    async fn legacy(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>, remote: &Caller) {
        let Some(refs) = ctx.state().get::<LegacyRefs>(LEGACY_REFS_KEY) else {
            return;
        };
        let private = ctx.random().random_bool(0.5);
        let target = if private {
            Target::LegacyPrivate
        } else {
            Target::LegacyPublic
        };
        // A credential is never written on a version 1 session: a call
        // carrying one fails before anything leaves.
        let kind = if ctx.random().random_bool(0.5) {
            Kind::Valid
        } else {
            Kind::Anonymous
        };
        let (id, _) = remote.issue(kind, target, 120, 0);
        let outcome = if private {
            let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private) else {
                return;
            };
            remote.unary(rpc, &service, id, 0).await
        } else {
            let Ok(service) = ServiceRef::<PublicEcho>::from_bytes(&refs.public) else {
                return;
            };
            remote.unary(rpc, &service, id, 0).await
        };
        let _ = remote.judge(id, &outcome.clone().map(Some));
        self.note(id, SecurityOp::Legacy, kind, &describe(&outcome));
    }

    /// After the script stopped (clock restored, server back), a fresh
    /// valid token must reach the private endpoint again.
    async fn recover(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        remote: &Caller,
    ) -> SimulationResult<()> {
        while !ctx.state().contains(SCRIPT_DONE_KEY) {
            if ctx.shutdown().is_cancelled() || pause(ctx, Duration::from_millis(50)).await.is_err()
            {
                return Ok(());
            }
        }
        for _ in 0..60 {
            if let Some(refs) = Self::server_refs(ctx)
                && let Ok(service) = ServiceRef::<PrivateEcho>::from_bytes(&refs.private)
            {
                let (id, _) = remote.issue(Kind::Valid, Target::Private, 300, 0);
                let outcome = remote.unary(rpc, &service, id, 0).await;
                let _ = remote.judge(id, &outcome.clone().map(Some));
                if outcome.is_ok() {
                    self.note(id, SecurityOp::Private, Kind::Valid, "recovered");
                    assert_sometimes!(true, "rpc security valid token served after the faults");
                    return Ok(());
                }
            }
            if pause(ctx, Duration::from_millis(250)).await.is_err() {
                return Ok(());
            }
        }
        assert_always!(
            false,
            "a valid token reaches the private endpoint once the faults stopped"
        );
        Ok(())
    }
}

#[async_trait]
impl Workload for SecurityWorkload {
    fn name(&self) -> &'static str {
        "rpc_security_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let config_error = |error| SimulationError::InvalidState(format!("rpc config: {error}"));
        let (driver, rpc) = RpcDriver::client_only(ctx.providers().clone(), client_config())
            .map_err(config_error)?;
        let (version1_driver, version1) = RpcDriver::client_only(
            ctx.providers().clone(),
            RpcConfig {
                protocol_versions: 1..=1,
                ..client_config()
            },
        )
        .map_err(config_error)?;
        let probes = [rpc.probe(), version1.probe()];
        let runtimes = Runtimes { rpc, version1 };
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            error = version1_driver.run() => {
                Err(SimulationError::IoError(format!("rpc driver: {error}")))
            }
            result = self.drive(ctx, &runtimes) => result,
        };
        for probe in probes.into_iter().flatten() {
            assert_always!(
                probe.is_released(),
                "the workload runtimes released everything once their drivers dropped"
            );
        }
        result
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let ledger = Ledger::of(ctx.state());
        for (id, issued, receipts, class) in ledger.all() {
            match class {
                Some(Class::Replied | Class::Executed) => {
                    assert_always!(
                        receipts >= 1,
                        "a request that produced an outcome reached its handler",
                        { "id" => id }
                    );
                }
                Some(Class::NotAdmitted) => {
                    assert_always!(
                        receipts == 0,
                        "a request reported not admitted never reached a handler",
                        { "id" => id }
                    );
                }
                Some(Class::Maybe) | None => {}
            }
            if issued.minted.kind != Kind::Refreshing {
                assert_always!(receipts <= 1, "an at-most-once request ran at most once");
            }
        }
        self.records
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(SecurityRecord {
                history: std::mem::take(&mut self.history),
            });
        Ok(())
    }
}
