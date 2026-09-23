//! The surviving workload: its runtimes, its operation alphabet and its
//! records. The operations are in [`super::ops`], the recovery phase, the
//! baselines and the end-of-run judgement in [`super::recovery`].

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::balance::{
    AlternativeSet, AttemptEvent, AttemptKind, BalanceConfig, BalanceHooks, BalancedClient,
    Feedback, HedgeBudget, Locality, ModelConfig, QueueModel, SetVersion, Verdict,
};
use moonpool_rpc::{
    AccessClass, BootstrapAddress, ErrorReason, Execution, RpcConfig, RpcDriver, RpcError,
    RpcHandle, WellKnownRef,
};
use moonpool_sim::{
    RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult, Workload,
    assert_always, assert_sometimes,
};

use super::messages::{DIRECTORY_ID, Directory, Done, Lookup, Publication, Work};
use super::state::{
    Instance, LANES_STOPPED_KEY, LaneRecord, QUIESCED_KEY, QualLedger, SCRIPT_DONE_KEY,
    VERSION1_LABEL, WORKLOAD_IP_KEY, WORKLOAD_LABEL, lane_key,
};
use super::{HostJitter, QualificationRecord, QualificationRecords, RPC_PORT, qual_config};
use crate::foundations::report_stats;
use crate::foundations::state::Board;
use crate::security::judge::Caller;
use crate::security::state::{Ledger as AuthLedger, Route};
use crate::security::trust::Trust;

/// One operation of the workload's alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QualOp {
    /// An at-most-once call to a server's `Work` instance (maybe stale),
    /// sometimes abandoned by its caller.
    Unary,
    /// A reliable call held across cuts and reboots, sometimes beside an
    /// active stream on the same session.
    Reliable,
    /// A reliable call bounded by sustained failure.
    UnlessFailed,
    /// A one-way request.
    OneWay,
    /// A reply stream: eager, slow, abandoned before or after its first
    /// item, or read until the caller gives up.
    Stream,
    /// A balanced call over the servers' current publications.
    Balanced,
    /// A credentialed call to a server's private endpoint.
    Auth,
    /// A call to a stored interface of an ended boot.
    Stale,
    /// A directory lookup.
    Lookup,
    /// Recruit members on two servers and hand them to the third
    /// participant.
    Recruit,
    /// Hand a server the third participant's callback (maybe stale).
    Callback,
    /// A call to the version 1 legacy peer.
    Legacy,
    /// A call from the version 1 only runtime to a verifying server.
    Version1,
    /// Many held calls to one server at once, then one more.
    Burst,
    /// A raw session that sends garbage to a server.
    Malformed,
    /// Held calls and an endless stream to one server, then ask the script
    /// to shut that server down gracefully under them.
    Shutdown,
}

impl QualOp {
    const ALL: [Self; 16] = [
        Self::Unary,
        Self::Reliable,
        Self::UnlessFailed,
        Self::OneWay,
        Self::Stream,
        Self::Balanced,
        Self::Auth,
        Self::Stale,
        Self::Lookup,
        Self::Recruit,
        Self::Callback,
        Self::Legacy,
        Self::Version1,
        Self::Burst,
        Self::Malformed,
        Self::Shutdown,
    ];
}

/// The workload's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationConfig {
    /// The most operations a run makes; the workload stops earlier, when
    /// the fault script is done (its operations all run under the faults).
    pub operations: usize,
    /// Relative weight per [`QualOp`], in declaration order.
    pub weights: [u32; 16],
    /// Pause between operations, in milliseconds (half-open range).
    pub gap_ms: (u64, u64),
    /// Run the reboot-storm script (one server restarted ten times or
    /// more) instead of the mixed one.
    pub storm: bool,
    /// Host-speed variation applied to every process and the workload.
    pub jitter: HostJitter,
}

impl QualificationConfig {
    /// The campaign mix.
    #[must_use]
    pub fn campaign() -> Self {
        Self {
            operations: 400,
            weights: [12, 9, 5, 3, 14, 12, 14, 5, 5, 4, 5, 3, 2, 3, 2, 3],
            gap_ms: (10, 150),
            storm: false,
            jitter: HostJitter::Off,
        }
    }

    /// The reboot storm: the same mix while one server restarts in place
    /// over and over.
    #[must_use]
    pub fn reboot_storm() -> Self {
        Self {
            storm: true,
            ..Self::campaign()
        }
    }

    /// The same configuration under host-speed variation.
    #[must_use]
    pub fn with_jitter(self, jitter: HostJitter) -> Self {
        Self { jitter, ..self }
    }
}

/// What an outcome proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Class {
    /// A reply came back.
    Replied,
    /// Refused before any handler.
    NotAdmitted,
    /// The handler ran, the outcome was unusable.
    Executed,
    /// Unknown: timeout, disconnect after sending, abandoned, one-way.
    Maybe,
}

pub(crate) fn classify<T>(outcome: &Result<T, RpcError>) -> Class {
    match outcome {
        Ok(_) => Class::Replied,
        Err(error) => match error.execution() {
            Execution::NotAdmitted => Class::NotAdmitted,
            Execution::Executed => Class::Executed,
            _ => Class::Maybe,
        },
    }
}

pub(crate) fn describe<T>(outcome: &Result<T, RpcError>) -> String {
    match outcome {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("{:?}/{:?}", error.reason(), error.execution()),
    }
}

/// One call to judge against the execution ledger at the end.
#[derive(Debug, Clone)]
pub(crate) struct CallRecord {
    pub(crate) id: u64,
    pub(crate) op: QualOp,
    /// The instance the reference named.
    pub(crate) expected: Instance,
    /// A single attempt: at most one execution.
    pub(crate) single: bool,
    pub(crate) class: Class,
}

/// The balanced-attempt ledger, fed by the observation hook.
#[derive(Default)]
pub(crate) struct Observer {
    pub(crate) started: AtomicU64,
    pub(crate) ended: AtomicU64,
    pub(crate) ended_late: AtomicU64,
    pub(crate) primaries: AtomicU64,
    pub(crate) copies: AtomicU64,
}

impl Observer {
    pub(crate) fn load(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

struct Hooks {
    observer: Arc<Observer>,
}

impl BalanceHooks<Work> for Hooks {
    fn classify(&self, _reply: &Done) -> Verdict {
        Verdict::Accept(Feedback::default())
    }

    fn observe(&self, event: &AttemptEvent) {
        match event {
            AttemptEvent::Started { kind, .. } => {
                self.observer.started.fetch_add(1, Ordering::Relaxed);
                let class = match kind {
                    AttemptKind::First | AttemptKind::Retry => &self.observer.primaries,
                    _ => &self.observer.copies,
                };
                class.fetch_add(1, Ordering::Relaxed);
            }
            AttemptEvent::Ended { late, .. } => {
                self.observer.ended.fetch_add(1, Ordering::Relaxed);
                if *late {
                    self.observer.ended_late.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// The balancer's queue model: a small hedge budget so it runs out, a
/// small late-loser bound, short exclusions.
pub(crate) fn model_config() -> ModelConfig {
    ModelConfig {
        hedge_budget: HedgeBudget {
            initial: 2.0,
            growth: 0.1,
            max: 3.0,
        },
        max_lagging: 8,
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

/// The workload's runtimes and clients.
pub(crate) struct Runtimes {
    /// Speaks versions 1 and 2, sends credentials over plaintext.
    pub(crate) rpc: RpcHandle<SimProviders>,
    /// Speaks version 1 only.
    pub(crate) version1: RpcHandle<SimProviders>,
    /// Balanced calls over `Work`.
    pub(crate) balanced: BalancedClient<SimProviders, Work>,
    /// Credentialed calls, judged by the security campaign's ledger.
    pub(crate) auth: Caller,
}

/// The campaign's workload: one lane of it. Lanes run the operation mix
/// concurrently, each with its own runtimes and balancer, sharing the
/// ledgers; lane 0 (the lead) also probes draining servers, runs the
/// recovery phase and the idle-baseline checks, and assembles the run's
/// record.
pub struct QualificationWorkload {
    pub(crate) lane: usize,
    lanes: usize,
    label: String,
    pub(crate) config: QualificationConfig,
    records: QualificationRecords,
    trace: Arc<Mutex<Vec<String>>>,
    pub(crate) history: Vec<String>,
    pub(crate) next_id: u64,
    pub(crate) next_configuration: u64,
    /// The latest base publication learned per process.
    pub(crate) known: BTreeMap<String, Publication>,
    /// Every publication ever learned (stored as data).
    pub(crate) stored: Vec<Publication>,
    pub(crate) calls: Vec<CallRecord>,
    pub(crate) streams: Vec<super::ops::StreamCheck>,
    pub(crate) balanced: Vec<super::ops::BalancedCheck>,
    pub(crate) set_version: u64,
    /// The instances of the installed alternative set.
    pub(crate) set_instances: Vec<Instance>,
    pub(crate) observer: Arc<Observer>,
    /// A credential from a retired key was refused; waiting for a fresh
    /// key's acceptance.
    pub(crate) retired_refused: bool,
    pub(crate) recovered_ms: Option<u64>,
    pub(crate) baseline: bool,
}

impl QualificationWorkload {
    /// Lane `lane` of `lanes`; the lead lane (0) appends the run's record
    /// to `records`, with the trace the campaign's capture fills.
    #[must_use]
    pub fn new(
        lane: usize,
        lanes: usize,
        config: QualificationConfig,
        records: QualificationRecords,
        trace: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            lane,
            lanes,
            label: format!("rpc_qualification_lane_{lane}"),
            config,
            records,
            trace,
            history: Vec::new(),
            next_id: 0,
            next_configuration: 0,
            known: BTreeMap::new(),
            stored: Vec::new(),
            calls: Vec::new(),
            streams: Vec::new(),
            balanced: Vec::new(),
            set_version: 0,
            set_instances: Vec::new(),
            observer: Arc::new(Observer::default()),
            retired_refused: false,
            recovered_ms: None,
            baseline: false,
        }
    }

    /// A job or stream id unique in the run (lanes have their own ranges).
    pub(crate) fn id(&mut self) -> u64 {
        self.next_id += 1;
        ((self.lane as u64 + 1) << 40) | self.next_id
    }

    /// A configuration number unique in the run.
    pub(crate) fn configuration(&mut self) -> u64 {
        self.next_configuration += 1;
        ((self.lane as u64 + 1) << 32) | self.next_configuration
    }

    fn is_lead(&self) -> bool {
        self.lane == 0
    }

    fn pick(&self, ctx: &SimContext) -> QualOp {
        let total: u32 = self.config.weights.iter().sum();
        let mut draw = ctx.random().random_range(0..total.max(1));
        for (op, weight) in QualOp::ALL.into_iter().zip(self.config.weights) {
            if draw < weight {
                return op;
            }
            draw -= weight;
        }
        QualOp::Unary
    }

    /// Record a call for the end-of-run judgement and the history.
    pub(crate) fn record<T>(
        &mut self,
        id: u64,
        op: QualOp,
        expected: &Instance,
        single: bool,
        outcome: &Result<T, RpcError>,
    ) -> Class {
        let class = classify(outcome);
        self.record_class(id, op, expected, single, class, &describe(outcome));
        class
    }

    /// Record a call whose class is known without an outcome (abandoned,
    /// queued one-way, relayed).
    pub(crate) fn record_class(
        &mut self,
        id: u64,
        op: QualOp,
        expected: &Instance,
        single: bool,
        class: Class,
        note: &str,
    ) {
        self.calls.push(CallRecord {
            id,
            op,
            expected: expected.clone(),
            single,
            class,
        });
        let line = format!("{id} {op:?} {expected} {note}");
        tracing::info!(line = %line, "rpc_qual_event");
        self.history.push(line);
    }

    /// Learn a publication: check it was published by the instance it
    /// names, remember it, and store it.
    pub(crate) fn learn(&mut self, ctx: &SimContext, publication: &Publication) {
        let ledger = QualLedger::of(ctx.state());
        let claimed = Instance {
            process: publication.server.clone(),
            boot: publication.boot,
            configuration: publication.configuration,
        };
        for bytes in [&publication.work, &publication.scan, &publication.private] {
            if bytes.is_empty() {
                continue;
            }
            assert_always!(
                ledger.publisher(bytes).as_ref() == Some(&claimed),
                "rpc qual a learned interface came from the instance it names",
                { "claimed" => claimed.to_string() }
            );
        }
        if publication.configuration == 0 {
            let republished = self.stored.iter().any(|old| {
                old.server == publication.server
                    && old.configuration == 0
                    && old.boot < publication.boot
            });
            if republished {
                assert_sometimes!(
                    true,
                    "rpc qual participant republished after a same-address reboot"
                );
            }
            self.known
                .insert(publication.server.clone(), publication.clone());
        }
        if !self.stored.contains(publication) {
            self.stored.push(publication.clone());
        }
    }

    /// Forget what we knew of `process` (a stale reference was refused):
    /// the next operation there looks it up again.
    pub(crate) fn forget(&mut self, process: &str) {
        self.known.remove(process);
    }

    /// Look a process's publication up through its well-known directory.
    pub(crate) async fn lookup(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        process: &str,
    ) -> Option<Publication> {
        let address = format!("{process}:{RPC_PORT}").parse::<SocketAddr>().ok()?;
        let directory = WellKnownRef::<Directory>::new(
            BootstrapAddress::Resolved(address),
            DIRECTORY_ID,
            AccessClass::Public,
        )
        .at(address)
        .bind(rpc);
        match directory
            .try_get_reply_within(&Lookup {}, Duration::from_secs(1))
            .await
        {
            Ok(publication) => {
                self.learn(ctx, &publication);
                self.history
                    .push(format!("lookup {process} boot {}", publication.boot));
                Some(publication)
            }
            Err(error) => {
                if *error.reason() == ErrorReason::EndpointNotFound {
                    assert_sometimes!(true, "rpc qual lookup reached a boot before it registered");
                }
                self.history
                    .push(format!("lookup {process} {}", describe::<()>(&Err(error))));
                None
            }
        }
    }

    /// The known publication of `process`, looked up when unknown.
    pub(crate) async fn publication(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        process: &str,
    ) -> Option<Publication> {
        if let Some(known) = self.known.get(process) {
            return Some(known.clone());
        }
        self.lookup(ctx, rpc, process).await
    }

    /// A random server's IP.
    pub(crate) fn random_server(ctx: &SimContext) -> Option<String> {
        let servers = ctx.topology().ips_in_group("server");
        if servers.is_empty() {
            return None;
        }
        Some(servers[ctx.random().random_range(0..servers.len())].clone())
    }

    /// Install a newer alternative set from the servers' known
    /// publications (looked up when unknown): the only way the balancer
    /// learns a new incarnation.
    pub(crate) async fn refresh(&mut self, ctx: &SimContext, runtimes: &Runtimes) {
        let mut alternatives = Vec::new();
        let mut instances = Vec::new();
        for server in ctx.topology().ips_in_group("server") {
            let Some(publication) = self.publication(ctx, &runtimes.rpc, &server).await else {
                continue;
            };
            let Ok(target) = moonpool_rpc::ServiceRef::<Work>::from_bytes(&publication.work) else {
                continue;
            };
            let datacenter = server
                .rsplit('.')
                .next()
                .and_then(|octet| octet.parse::<u8>().ok())
                .map_or(0, |octet| octet % 2);
            alternatives.push(moonpool_rpc::balance::Alternative::new(
                target,
                Locality::new(server.clone(), format!("dc{datacenter}")),
            ));
            instances.push(Instance {
                process: server,
                boot: publication.boot,
                configuration: 0,
            });
        }
        self.set_version += 1;
        match AlternativeSet::new(SetVersion::new(self.set_version), alternatives) {
            Ok(set) => {
                let replaced = runtimes.balanced.replace(set);
                assert_always!(
                    replaced.is_ok(),
                    "a newer alternative set is always installed"
                );
                self.set_instances = instances;
                self.history.push(format!(
                    "refresh v{} {}",
                    self.set_version,
                    self.set_instances
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            Err(error) => {
                assert_always!(false, "published references form a valid set", {
                    "error" => error.to_string()
                });
            }
        }
    }

    async fn drive(&mut self, ctx: &SimContext, runtimes: &Runtimes) -> SimulationResult<()> {
        let mut ips = ctx
            .state()
            .get::<Vec<String>>(WORKLOAD_IP_KEY)
            .unwrap_or_default();
        ips.push(ctx.my_ip().to_string());
        ctx.state().publish(WORKLOAD_IP_KEY, ips);
        let done = std::sync::atomic::AtomicBool::new(!self.is_lead());
        let watcher = super::recovery::drain_watcher(ctx, &runtimes.rpc, &done);
        let operations = async {
            for _ in 0..self.config.operations {
                if ctx.shutdown().is_cancelled() || ctx.state().contains(SCRIPT_DONE_KEY) {
                    break;
                }
                let op = self.pick(ctx);
                self.step(op, ctx, runtimes).await;
                let (low, high) = self.config.gap_ms;
                let gap = ctx.random().random_range(low..high.max(low + 1));
                if super::pause(ctx, Duration::from_millis(gap)).await.is_err() {
                    break;
                }
            }
            done.store(true, Ordering::Relaxed);
        };
        let ((), (drain_lines, drain_calls)) = futures::join!(operations, watcher);
        self.history.extend(drain_lines);
        self.calls.extend(drain_calls);
        if ctx.shutdown().is_cancelled() {
            return Ok(());
        }
        self.drain_balancer(ctx, runtimes).await;
        let lanes_stopped = ctx.state().get::<u64>(LANES_STOPPED_KEY).unwrap_or(0);
        ctx.state().publish(LANES_STOPPED_KEY, lanes_stopped + 1);
        if self.is_lead() {
            self.recover(ctx, runtimes).await;
            // Every other lane stopped issuing work before the baseline.
            let lane_count = self.lanes as u64;
            for _ in 0..2400 {
                if ctx.state().get::<u64>(LANES_STOPPED_KEY).unwrap_or(0) >= lane_count {
                    break;
                }
                if super::pause(ctx, Duration::from_millis(50)).await.is_err() {
                    return Ok(());
                }
            }
            self.quiesce(ctx, runtimes).await;
            ctx.state().publish(QUIESCED_KEY, true);
        } else {
            for _ in 0..4800 {
                if ctx.state().contains(QUIESCED_KEY) {
                    break;
                }
                if super::pause(ctx, Duration::from_millis(50)).await.is_err() {
                    return Ok(());
                }
            }
            self.client_idle(runtimes);
        }
        self.judge_all(ctx);
        Ok(())
    }
}

/// The workload runtimes' configuration: the simulation's sessions are
/// plaintext, so they opt into sending credentials over them explicitly.
fn client_config() -> RpcConfig {
    let config = qual_config();
    RpcConfig {
        security: config.security.clone().send_credentials_over_plaintext(),
        ..config
    }
}

#[async_trait]
impl Workload for QualificationWorkload {
    fn name(&self) -> &str {
        &self.label
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
        let probes = [rpc.probe(), version1.probe()];
        let label = format!("{WORKLOAD_LABEL}-{}", self.lane);
        if let Some(probe) = &probes[0] {
            board.register_probe(&label, probe.clone());
        }
        if let Some(probe) = &probes[1] {
            board.register_probe(&format!("{VERSION1_LABEL}-{}", self.lane), probe.clone());
        }
        let auth = Caller {
            ledger: AuthLedger::of(ctx.state()),
            trust: Trust::of(ctx.state())?,
            route: Route::Remote,
        };
        let runtimes = Runtimes {
            rpc: rpc.clone(),
            version1,
            balanced,
            auth,
        };
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            error = version1_driver.run() => {
                Err(SimulationError::IoError(format!("rpc driver: {error}")))
            }
            result = self.drive(ctx, &runtimes) => result,
            () = model.collect_lagging() => Ok(()),
            () = report_stats(&rpc, &board, &label, ctx) => Ok(()),
        };
        drop(runtimes);
        drop(rpc);
        let at_baseline = probes
            .iter()
            .flatten()
            .all(moonpool_rpc::ResourceProbe::is_at_baseline);
        assert_always!(
            at_baseline,
            "rpc qual the lane runtimes are at baseline after their drivers"
        );
        self.baseline = at_baseline;
        ctx.state().publish(
            &lane_key(self.lane),
            LaneRecord {
                history: std::mem::take(&mut self.history),
                baseline: at_baseline,
            },
        );
        result
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        if !self.is_lead() {
            return Ok(());
        }
        let ledger = QualLedger::of(ctx.state());
        let board = Board::of(ctx.state());
        // Every runtime of an ended boot is back at its baseline (the
        // current boots are still running).
        let mut ended_at_baseline = true;
        for role in ["server", "third", "legacy"] {
            for process in ctx.topology().ips_in_group(role) {
                let current = format!("{role}@{process}#{:04}", ledger.current_boot(&process));
                for (label, probe) in board.probes(&format!("{role}@{process}#"), &current) {
                    let released = probe.is_at_baseline();
                    ended_at_baseline &= released;
                    assert_always!(
                        released,
                        "rpc qual every runtime of the run returned to baseline",
                        { "runtime" => label, "outstanding" => format!("{:?}", probe.outstanding()) }
                    );
                }
            }
        }
        let mut history = Vec::new();
        let mut baseline = ended_at_baseline;
        for lane in 0..self.lanes {
            let record = ctx
                .state()
                .get::<LaneRecord>(&lane_key(lane))
                .unwrap_or_default();
            baseline &= record.baseline;
            history.extend(
                record
                    .history
                    .into_iter()
                    .map(|line| format!("[{lane}] {line}")),
            );
        }
        let trace = std::mem::take(
            &mut *self
                .trace
                .lock()
                .expect("Mutex poisoned: prior task panicked"),
        );
        super::recovery::check_redaction(ctx, &trace);
        let record = QualificationRecord {
            history,
            ledger: ledger.digest(),
            trace,
            recovered_ms: self.recovered_ms,
            baseline,
        };
        self.records
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(record);
        Ok(())
    }
}
