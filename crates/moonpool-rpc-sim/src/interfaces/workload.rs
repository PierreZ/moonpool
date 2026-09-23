//! The surviving client ("B"): learns participants' interfaces only
//! through their directories and recruiters, calls them, keeps stored
//! publications across restarts, recruits members and forwards them to
//! the third participant, and judges everything against the ledger.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use moonpool_rpc::{
    AccessClass, BootstrapAddress, ErrorReason, Execution, RpcDriver, RpcError, RpcHandle,
    RpcMethod, ServiceClient, WellKnownId, WellKnownRef,
};
use moonpool_sim::{
    RandomProvider, SimContext, SimProviders, SimulationError, SimulationResult, TimeProvider,
    Workload, assert_always, assert_sometimes,
};
use prost::Message;

use super::messages::{
    ADOPTER_ID, Adopt, Adopted, Adopter, DIRECTORY_ID, DISMISS_ID, Directory, Dismiss, Lookup,
    Probe, Publication, RECRUITER_ID, Recruit, Recruiter, RoleClient,
};
use super::state::{Instance, Ledger, SCRIPT_DONE_KEY, WORKLOAD_LABEL};
use super::{InterfacesRecord, InterfacesRecords};
use crate::foundations::state::Board;
use crate::foundations::{RPC_PORT, report_stats, rpc_config};

/// One operation of the workload's alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InterfaceOp {
    /// Ask a participant's directory for its current interface.
    Lookup,
    /// Call the base instance through the learned interface.
    Call,
    /// A one-way note through the learned interface.
    OneWay,
    /// Reliable delivery through the learned interface.
    Reliable,
    /// Decode a stored publication (possibly of an ended boot) and call it.
    Stored,
    /// Recruit members for a new configuration and have the third
    /// participant call them.
    Recruit,
    /// Call a recruited member directly.
    Direct,
    /// Dismiss a recruited member, then call it.
    Dismiss,
}

impl InterfaceOp {
    const ALL: [Self; 8] = [
        Self::Lookup,
        Self::Call,
        Self::OneWay,
        Self::Reliable,
        Self::Stored,
        Self::Recruit,
        Self::Direct,
        Self::Dismiss,
    ];
}

/// The workload's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfacesConfig {
    /// Operations per run.
    pub operations: usize,
    /// Relative weight per [`InterfaceOp`], in declaration order.
    pub weights: [u32; 8],
    /// Pause between operations, in milliseconds (half-open range).
    pub gap_ms: (u64, u64),
}

impl InterfacesConfig {
    /// The full campaign mix.
    #[must_use]
    pub fn campaign() -> Self {
        Self {
            operations: 50,
            weights: [10, 20, 6, 8, 10, 10, 8, 5],
            gap_ms: (20, 300),
        }
    }
}

/// What an outcome proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Replied,
    NotAdmitted,
    Maybe,
}

fn classify<T>(outcome: &Result<T, RpcError>) -> Class {
    match outcome {
        Ok(_) => Class::Replied,
        Err(error) if error.execution() == Execution::NotAdmitted => Class::NotAdmitted,
        Err(error) if error.execution() == Execution::Executed => Class::Replied,
        Err(_) => Class::Maybe,
    }
}

fn describe<T>(outcome: &Result<T, RpcError>) -> String {
    match outcome {
        Ok(_) => "ok".to_string(),
        Err(error) => format!("{:?}/{:?}", error.reason(), error.execution()),
    }
}

fn is_stale(outcome: &Result<impl Sized, RpcError>) -> bool {
    matches!(outcome, Err(error) if *error.reason() == ErrorReason::StaleIncarnation)
}

/// Deadline of calls that should normally complete.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Bound on a reliable call.
const RELIABLE_TIMEOUT: Duration = Duration::from_secs(10);

/// One judged call: its probe id, the instance its reference named, and
/// what its outcome proved.
struct Call {
    id: u64,
    expected: Instance,
    single_attempt: bool,
    class: Class,
}

/// The campaign's workload.
pub struct InterfacesWorkload {
    config: InterfacesConfig,
    records: InterfacesRecords,
    history: Vec<String>,
    calls: Vec<Call>,
    next_id: u64,
    next_configuration: u64,
    /// The latest base publication learned per participant.
    known: BTreeMap<String, Publication>,
    /// Every publication ever learned, stored encoded (as an application
    /// would persist it) — including those of ended boots.
    stored: Vec<Vec<u8>>,
    /// Members recruited and not dismissed.
    recruited: Vec<Publication>,
    /// Participants whose learned interface was refused as stale, until a
    /// lookup replaced it.
    refused: BTreeMap<String, u64>,
}

impl InterfacesWorkload {
    /// A fresh workload appending its run record to `records`.
    #[must_use]
    pub fn new(config: InterfacesConfig, records: InterfacesRecords) -> Self {
        Self {
            config,
            records,
            history: Vec::new(),
            calls: Vec::new(),
            next_id: 0,
            next_configuration: 0,
            known: BTreeMap::new(),
            stored: Vec::new(),
            recruited: Vec::new(),
            refused: BTreeMap::new(),
        }
    }

    fn pick(&self, ctx: &SimContext) -> InterfaceOp {
        let total: u32 = self.config.weights.iter().sum();
        let mut draw = ctx.random().random_range(0..total.max(1));
        for (op, weight) in InterfaceOp::ALL.into_iter().zip(self.config.weights) {
            if draw < weight {
                return op;
            }
            draw -= weight;
        }
        InterfaceOp::Call
    }

    fn probe(&mut self, publication: &Publication) -> Probe {
        self.next_id += 1;
        Probe {
            id: self.next_id,
            participant: publication.participant.clone(),
            boot: publication.boot,
            configuration: publication.configuration,
        }
    }

    fn record<T>(
        &mut self,
        op: InterfaceOp,
        probe: &Probe,
        single_attempt: bool,
        outcome: &Result<T, RpcError>,
    ) {
        self.push_call(
            op,
            probe,
            single_attempt,
            classify(outcome),
            &describe(outcome),
        );
    }

    fn push_call(
        &mut self,
        op: InterfaceOp,
        probe: &Probe,
        single_attempt: bool,
        class: Class,
        description: &str,
    ) {
        self.calls.push(Call {
            id: probe.id,
            expected: Instance {
                participant: probe.participant.clone(),
                boot: probe.boot,
                configuration: probe.configuration,
            },
            single_attempt,
            class,
        });
        self.history
            .push(format!("{} {op:?} {description}", probe.id));
    }

    /// Check a publication against the independent publication ledger:
    /// the boot and configuration it names really published it.
    fn learned(ctx: &SimContext, publication: &Publication) -> bool {
        let Some(role) = &publication.role else {
            return false;
        };
        let publisher = Ledger::of(ctx.state()).publisher(&role.to_bytes());
        let expected = Instance {
            participant: publication.participant.clone(),
            boot: publication.boot,
            configuration: publication.configuration,
        };
        assert_always!(
            publisher.as_ref() == Some(&expected),
            "every learned interface was published by the instance it names"
        );
        true
    }

    /// Judge a probe's outcome on the spot.
    fn judge(
        ctx: &SimContext,
        publication: &Publication,
        outcome: &Result<super::messages::Answer, RpcError>,
    ) {
        match outcome {
            Ok(answer) => {
                assert_always!(
                    answer.participant == publication.participant
                        && answer.boot == publication.boot
                        && answer.configuration == publication.configuration,
                    "a reply comes from the instance that published the interface"
                );
                assert_sometimes!(
                    true,
                    "rpc call reached the incarnation that published its interface"
                );
            }
            Err(error) if *error.reason() == ErrorReason::StaleIncarnation => {
                let current = Ledger::of(ctx.state()).current_boot(&publication.participant);
                assert_always!(
                    current > publication.boot,
                    "a stale refusal names an incarnation that really ended",
                    { "boot" => publication.boot, "current" => current }
                );
                assert_sometimes!(
                    true,
                    "rpc stale interface refused after a same-address restart"
                );
            }
            Err(_) => {}
        }
    }

    fn well_known<M: RpcMethod>(
        rpc: &RpcHandle<SimProviders>,
        participant: &str,
        id: WellKnownId,
    ) -> Option<ServiceClient<SimProviders, M>> {
        let address: SocketAddr = format!("{participant}:{RPC_PORT}").parse().ok()?;
        Some(
            WellKnownRef::<M>::new(BootstrapAddress::Resolved(address), id, AccessClass::Public)
                .at(address)
                .bind(rpc),
        )
    }

    /// Learn a participant's current base interface through its directory,
    /// retrying (a lookup is idempotent).
    async fn lookup(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        participant: &str,
    ) -> Option<Publication> {
        let directory = Self::well_known::<Directory>(rpc, participant, DIRECTORY_ID)?;
        for _ in 0..20 {
            let outcome = directory
                .try_get_reply_within(&Lookup {}, Duration::from_millis(1500))
                .await;
            self.history
                .push(format!("lookup {participant} {}", describe(&outcome)));
            match outcome {
                Ok(publication) if Self::learned(ctx, &publication) => {
                    self.adopt(publication.clone());
                    return Some(publication);
                }
                Ok(_) => return None,
                Err(error) => {
                    if *error.reason() == ErrorReason::EndpointNotFound {
                        assert_sometimes!(
                            true,
                            "rpc directory lookup reached a boot before it registered"
                        );
                    }
                    let _ = ctx
                        .time()
                        .sleep(Duration::from_millis(ctx.random().random_range(50..300)))
                        .await;
                }
            }
        }
        None
    }

    /// Adopt a publication learned from a directory: the only way a new
    /// incarnation's interface becomes known.
    fn adopt(&mut self, publication: Publication) {
        let participant = publication.participant.clone();
        if let Some(previous) = self.known.get(&participant) {
            assert_always!(
                publication.boot >= previous.boot,
                "a directory never republishes a boot older than one learned"
            );
            if publication.boot > previous.boot {
                assert_sometimes!(true, "rpc new incarnation learned through republication");
            }
        }
        if let Some(refused_boot) = self.refused.get(&participant)
            && publication.boot > *refused_boot
        {
            self.refused.remove(&participant);
            assert_sometimes!(
                true,
                "rpc stale interface replaced only by an explicit lookup"
            );
        }
        self.stored.push(publication.encode_to_vec());
        self.known.insert(participant, publication);
    }

    fn random_participant(ctx: &SimContext) -> Option<String> {
        let participants = ctx.topology().ips_in_group("participant");
        if participants.is_empty() {
            return None;
        }
        Some(participants[ctx.random().random_range(0..participants.len())].clone())
    }

    /// The learned interface of a random participant, looking it up first
    /// if nothing is known yet.
    async fn target(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
    ) -> Option<Publication> {
        let participant = Self::random_participant(ctx)?;
        match self.known.get(&participant) {
            Some(publication) => Some(publication.clone()),
            None => self.lookup(ctx, rpc, &participant).await,
        }
    }

    /// One at-most-once status call through `publication`.
    async fn status(
        &mut self,
        ctx: &SimContext,
        rpc: &RpcHandle<SimProviders>,
        op: InterfaceOp,
        publication: &Publication,
    ) -> Option<Result<super::messages::Answer, RpcError>> {
        let role = publication.role.as_ref()?;
        let probe = self.probe(publication);
        let outcome = match RoleClient::bind(role, rpc) {
            Ok(client) => {
                client
                    .status()
                    .try_get_reply_within(&probe, CALL_TIMEOUT)
                    .await
            }
            Err(error) => Err(error),
        };
        Self::judge(ctx, publication, &outcome);
        self.record(op, &probe, true, &outcome);
        Some(outcome)
    }

    async fn call(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        let Some(publication) = self.target(ctx, rpc).await else {
            return;
        };
        let fast_before = rpc.stats().map_or(0, |stats| stats.calls_failed_fast);
        let Some(outcome) = self.status(ctx, rpc, InterfaceOp::Call, &publication).await else {
            return;
        };
        if is_stale(&outcome) {
            self.refused
                .insert(publication.participant.clone(), publication.boot);
            if rpc.stats().map_or(0, |stats| stats.calls_failed_fast) > fast_before {
                assert_sometimes!(
                    true,
                    "rpc stale interface failed fast after its first refusal"
                );
            }
            // The new incarnation is learned explicitly, never refreshed.
            let _ = self.lookup(ctx, rpc, &publication.participant).await;
        }
    }

    async fn one_way(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        let Some(publication) = self.target(ctx, rpc).await else {
            return;
        };
        let Some(role) = publication.role.as_ref() else {
            return;
        };
        let probe = self.probe(&publication);
        let sent = RoleClient::bind(role, rpc).and_then(|client| client.note().send(&probe));
        // `Ok` only means queued: the note runs zero or one times.
        match &sent {
            Ok(()) => self.push_call(InterfaceOp::OneWay, &probe, true, Class::Maybe, "queued"),
            Err(_) => self.record(InterfaceOp::OneWay, &probe, true, &sent),
        }
    }

    async fn reliable(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        let Some(publication) = self.target(ctx, rpc).await else {
            return;
        };
        let Some(role) = publication.role.as_ref() else {
            return;
        };
        let probe = self.probe(&publication);
        let outcome = match RoleClient::bind(role, rpc) {
            Ok(client) => {
                let status = client.status();
                let call = status.get_reply_unless_failed_for(&probe, Duration::from_secs(4), 0.0);
                ctx.time()
                    .timeout(RELIABLE_TIMEOUT, call)
                    .await
                    .unwrap_or_else(|_| {
                        Err(RpcError::new(
                            ErrorReason::Timeout,
                            Execution::MaybeExecuted,
                        ))
                    })
            }
            Err(error) => Err(error),
        };
        Self::judge(ctx, &publication, &outcome);
        if is_stale(&outcome) {
            assert_sometimes!(true, "rpc stale reliable call ended terminally");
            self.refused
                .insert(publication.participant.clone(), publication.boot);
        }
        self.record(InterfaceOp::Reliable, &probe, false, &outcome);
    }

    /// Decode a stored publication and call it: of the current boot it
    /// answers, of an ended boot it is refused.
    async fn stored(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        if self.stored.is_empty() {
            return;
        }
        let blob = self.stored[ctx.random().random_range(0..self.stored.len())].clone();
        let Ok(publication) = Publication::decode(blob.as_slice()) else {
            assert_always!(false, "a stored publication decodes without a runtime");
            return;
        };
        let current = Ledger::of(ctx.state()).current_boot(&publication.participant);
        let Some(outcome) = self
            .status(ctx, rpc, InterfaceOp::Stored, &publication)
            .await
        else {
            return;
        };
        if publication.boot < current && is_stale(&outcome) {
            assert_sometimes!(true, "rpc stored interface of an ended boot refused");
        }
    }

    async fn recruit(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        let participants = ctx.topology().ips_in_group("participant");
        let Some(third) = ctx.topology().ips_in_group("third").into_iter().next() else {
            return;
        };
        self.next_configuration += 1;
        let configuration = self.next_configuration;
        let wanted = ctx.random().random_range(1..3usize).min(participants.len());
        let first = ctx.random().random_range(0..participants.len().max(1));
        let mut members = Vec::new();
        for offset in 0..wanted {
            let participant = &participants[(first + offset) % participants.len()];
            let Some(recruiter) = Self::well_known::<Recruiter>(rpc, participant, RECRUITER_ID)
            else {
                continue;
            };
            let outcome = recruiter
                .try_get_reply_within(&Recruit { configuration }, CALL_TIMEOUT)
                .await;
            self.history.push(format!(
                "recruit {participant} {configuration} {}",
                describe(&outcome)
            ));
            if let Ok(member) = outcome
                && member.configuration == configuration
                && Self::learned(ctx, &member)
            {
                self.stored.push(member.encode_to_vec());
                members.push(member);
            }
        }
        if members.is_empty() {
            return;
        }
        self.recruited.extend(members.iter().cloned());
        let probes: Vec<Probe> = members.iter().map(|member| self.probe(member)).collect();
        let Some(adopter) = Self::well_known::<Adopter>(rpc, &third, ADOPTER_ID) else {
            return;
        };
        let request = Adopt {
            configuration,
            members: members.clone(),
            ids: probes.iter().map(|probe| probe.id).collect(),
        };
        let outcome = adopter
            .try_get_reply_within(&request, CALL_TIMEOUT * 3)
            .await;
        self.history
            .push(format!("adopt {configuration} {}", describe(&outcome)));
        self.judge_adoption(&members, &probes, outcome);
    }

    fn judge_adoption(
        &mut self,
        members: &[Publication],
        probes: &[Probe],
        outcome: Result<Adopted, RpcError>,
    ) {
        let Ok(adopted) = outcome else {
            // The third participant may or may not have called anyone.
            for probe in probes {
                self.push_call(
                    InterfaceOp::Recruit,
                    probe,
                    true,
                    Class::Maybe,
                    "adopt-failed",
                );
            }
            return;
        };
        for (member, probe) in members.iter().zip(probes) {
            let answer = adopted.answers.iter().find(|answer| answer.id == probe.id);
            let failure = adopted
                .failures
                .iter()
                .find(|failure| failure.split(':').next() == Some(&probe.id.to_string()));
            let (class, description) = match (answer, failure) {
                (Some(answer), _) => {
                    assert_always!(
                        answer.participant == member.participant
                            && answer.boot == member.boot
                            && answer.configuration == member.configuration,
                        "a recruited member answered for its recruited configuration"
                    );
                    assert_sometimes!(
                        true,
                        "rpc recruited interface invoked by a third participant"
                    );
                    (Class::Replied, "ok")
                }
                (None, Some(failure)) if failure.ends_with("/NotAdmitted") => {
                    (Class::NotAdmitted, failure.as_str())
                }
                (None, Some(failure)) => (Class::Maybe, failure.as_str()),
                (None, None) => (Class::Maybe, "missing"),
            };
            self.push_call(InterfaceOp::Recruit, probe, true, class, description);
        }
    }

    async fn direct(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        if self.recruited.is_empty() {
            return;
        }
        let member = self.recruited[ctx.random().random_range(0..self.recruited.len())].clone();
        let Some(outcome) = self.status(ctx, rpc, InterfaceOp::Direct, &member).await else {
            return;
        };
        if is_stale(&outcome) {
            assert_sometimes!(
                true,
                "rpc recruited interface refused after its participant restarted"
            );
        }
    }

    async fn dismiss(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        if self.recruited.is_empty() {
            return;
        }
        let member = self
            .recruited
            .remove(ctx.random().random_range(0..self.recruited.len()));
        let Some(dismiss) = Self::well_known::<Dismiss>(rpc, &member.participant, DISMISS_ID)
        else {
            return;
        };
        let outcome = dismiss
            .try_get_reply_within(
                &Recruit {
                    configuration: member.configuration,
                },
                CALL_TIMEOUT,
            )
            .await;
        self.history.push(format!(
            "dismiss {} {} {}",
            member.participant,
            member.configuration,
            describe(&outcome)
        ));
        let Ok(dismissed) = outcome else {
            return;
        };
        let Some(after) = self.status(ctx, rpc, InterfaceOp::Dismiss, &member).await else {
            return;
        };
        if dismissed.removed {
            assert_always!(after.is_err(), "a dismissed instance never serves again");
            if matches!(&after, Err(error) if *error.reason() == ErrorReason::EndpointNotFound) {
                assert_sometimes!(true, "rpc dismissed recruited interface refused");
            }
        }
    }

    async fn drive(&mut self, ctx: &SimContext, rpc: &RpcHandle<SimProviders>) {
        for _ in 0..self.config.operations {
            let gap = ctx
                .random()
                .random_range(self.config.gap_ms.0..self.config.gap_ms.1);
            let _ = ctx.time().sleep(Duration::from_millis(gap)).await;
            match self.pick(ctx) {
                InterfaceOp::Lookup => {
                    if let Some(participant) = Self::random_participant(ctx) {
                        let _ = self.lookup(ctx, rpc, &participant).await;
                    }
                }
                InterfaceOp::Call => self.call(ctx, rpc).await,
                InterfaceOp::OneWay => self.one_way(ctx, rpc).await,
                InterfaceOp::Reliable => self.reliable(ctx, rpc).await,
                InterfaceOp::Stored => self.stored(ctx, rpc).await,
                InterfaceOp::Recruit => self.recruit(ctx, rpc).await,
                InterfaceOp::Direct => self.direct(ctx, rpc).await,
                InterfaceOp::Dismiss => self.dismiss(ctx, rpc).await,
            }
        }
        // After the faults: every participant is reachable again through
        // an interface learned from its directory.
        for _ in 0..1200 {
            if ctx.state().get::<bool>(SCRIPT_DONE_KEY).unwrap_or(false) {
                break;
            }
            let _ = ctx.time().sleep(Duration::from_millis(50)).await;
        }
        let _ = ctx.time().sleep(Duration::from_secs(2)).await;
        let mut reachable = true;
        for participant in ctx.topology().ips_in_group("participant") {
            let mut answered = false;
            for _ in 0..3 {
                if let Some(publication) = self.lookup(ctx, rpc, &participant).await
                    && matches!(
                        self.status(ctx, rpc, InterfaceOp::Call, &publication).await,
                        Some(Ok(_))
                    )
                {
                    answered = true;
                    break;
                }
            }
            reachable &= answered;
        }
        assert_sometimes!(
            reachable,
            "rpc every participant answered a fresh publication at the end"
        );
    }
}

#[async_trait]
impl Workload for InterfacesWorkload {
    fn name(&self) -> &'static str {
        "rpc_interfaces_client"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let (driver, rpc) = RpcDriver::client_only(ctx.providers().clone(), rpc_config())
            .map_err(|error| SimulationError::InvalidState(format!("rpc config: {error}")))?;
        let board = Board::of(ctx.state());
        let probe = rpc.probe();
        if let Some(probe) = &probe {
            board.register_probe(WORKLOAD_LABEL, probe.clone());
        }
        let result = moonpool_sim::select! {
            error = driver.run() => Err(SimulationError::IoError(format!("rpc driver: {error}"))),
            () = self.drive(ctx, &rpc) => Ok(()),
            () = report_stats(&rpc, &board, WORKLOAD_LABEL, ctx) => Ok(()),
        };
        if let Some(probe) = &probe {
            assert_always!(
                probe.is_released(),
                "the interfaces workload runtime released everything at the end"
            );
        }
        result
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let ledger = Ledger::of(ctx.state());
        let mut one_way_executed = false;
        for call in &self.calls {
            let executions = ledger.executions(call.id);
            // Independently of the in-handler check: every run of a probe
            // happened in the instance its caller's reference named.
            assert_always!(
                executions.iter().all(|run| *run == call.expected),
                "every execution happened in the instance its reference named"
            );
            if call.single_attempt {
                assert_always!(
                    executions.len() <= 1,
                    "a single attempt through an interface never runs twice"
                );
            }
            match call.class {
                Class::Replied => {
                    assert_always!(!executions.is_empty(), "a replied interface call executed");
                }
                Class::NotAdmitted => {
                    assert_always!(
                        executions.is_empty(),
                        "an interface call reported not admitted never executed"
                    );
                }
                Class::Maybe => {}
            }
            if call.single_attempt && call.class == Class::Maybe && !executions.is_empty() {
                one_way_executed = true;
            }
        }
        assert_sometimes!(
            one_way_executed,
            "rpc ambiguous interface call executed in its published instance"
        );
        assert_sometimes!(
            ledger.restarts() >= 2,
            "rpc participants restarted repeatedly at the same address"
        );
        self.records
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(InterfacesRecord {
                history: std::mem::take(&mut self.history),
                restarts: ledger.restarts(),
            });
        Ok(())
    }
}
