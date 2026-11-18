//! Canonical model-based trace checking example: each seed's run of a
//! two-phase commit is checked against a TLA+ spec with TLC.
//!
//! 1. The code under test is a small sans-I/O two-phase commit: a
//!    transaction manager (TM) and three resource managers (RMs), each a
//!    plain state machine. A seeded workload is their network: at each step
//!    it picks one enabled action, and it drops or repeats messages.
//! 2. The code emits plain `tracing` events after each step, with the state
//!    of the actor after the step (`tracing::info!(rm = %r, state = %s,
//!    "rm_prepare")`). The harness also logs each fault it injects
//!    (`msg_drop`, `msg_duplicate`), so TLC guesses nothing.
//! 3. In `check()`, the workload turns the timeline into a TLA+ trace and
//!    asks TLC whether the run is a behaviour of `tests/tla/two_phase/
//!    TwoPhase.tla` (Lamport's spec), through the trace spec
//!    `TwoPhaseTrace.tla`.
//!
//! Happy path: every seed is accepted.
//!
//! Controls: three mutants of the code, each one a bug the spec forbids.
//! Each must give a rejected trace on some seed: the code took a step that
//! the spec does not allow. A code bug always shows as a rejection, because
//! every step of the trace spec is a step of the model, and the model keeps
//! its own invariants. So a coordinator that commits after the first vote
//! is caught even on seeds where nobody aborts, where no oracle that looks
//! only at the outcome (no resource manager commits while another aborts)
//! could see it.
//!
//! The tests need TLC (`tlc`, in the nix dev shell). Without it they are
//! skipped, except in CI, where they fail.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use moonpool_sim::observability::tla::trace_from_timeline;
use moonpool_sim::{
    RandomProvider, SimContext, SimulationBuilder, SimulationError, SimulationResult, TimeProvider,
    Workload,
};
use moonpool_tla::{TraceCheck, Value, Verdict};

/// The resource managers.
const RMS: [&str; 3] = ["r1", "r2", "r3"];

/// Every event the code and the harness emit, in the trace.
const EVENTS: [&str; 9] = [
    "rm_prepare",
    "rm_choose_abort",
    "rm_rcv_commit",
    "rm_rcv_abort",
    "tm_rcv_prepared",
    "tm_commit",
    "tm_abort",
    "msg_drop",
    "msg_duplicate",
];

/// The most steps one run takes.
const MAX_STEPS: usize = 60;

/// A bug planted in the code. Each one mirrors a step the spec forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutant {
    /// The code as it should be.
    None,
    /// The TM commits as soon as one RM is prepared.
    CommitOnFirstVote,
    /// A prepared RM commits without a commit message.
    CommitWithoutMessage,
    /// The TM's timeout aborts even after it committed.
    AbortAfterCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RmState {
    Working,
    Prepared,
    Committed,
    Aborted,
}

impl RmState {
    fn name(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmState {
    Init,
    Committed,
    Aborted,
}

impl TmState {
    fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

/// A message in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Msg {
    /// From an RM to the TM.
    Prepared(&'static str),
    /// From the TM to one RM.
    Commit(&'static str),
    /// From the TM to one RM.
    Abort(&'static str),
}

impl Msg {
    fn kind(self) -> &'static str {
        match self {
            Self::Prepared(_) => "prepared",
            Self::Commit(_) => "commit",
            Self::Abort(_) => "abort",
        }
    }

    fn rm(self) -> &'static str {
        match self {
            Self::Prepared(rm) | Self::Commit(rm) | Self::Abort(rm) => rm,
        }
    }
}

/// One step the network can take.
#[derive(Debug, Clone, Copy)]
enum Action {
    Prepare(&'static str),
    ChooseAbort(&'static str),
    SelfCommit(&'static str),
    Deliver(usize),
    Timeout,
}

/// The TM and the RMs: the code under test.
struct TwoPhase {
    mutant: Mutant,
    tm: TmState,
    prepared: Vec<&'static str>,
    rms: BTreeMap<&'static str, RmState>,
}

impl TwoPhase {
    fn new(mutant: Mutant) -> Self {
        Self {
            mutant,
            tm: TmState::Init,
            prepared: Vec::new(),
            rms: RMS.iter().map(|&rm| (rm, RmState::Working)).collect(),
        }
    }

    fn set_rm(&mut self, rm: &'static str, state: RmState) {
        self.rms.insert(rm, state);
    }

    fn rm(&self, rm: &str) -> RmState {
        self.rms.get(rm).copied().unwrap_or(RmState::Working)
    }

    /// RM `rm` votes yes.
    fn prepare(&mut self, rm: &'static str, out: &mut Vec<Msg>) {
        self.set_rm(rm, RmState::Prepared);
        tracing::info!(rm = %rm, state = %self.rm(rm).name(), "rm_prepare");
        out.push(Msg::Prepared(rm));
    }

    /// RM `rm` gives up before it votes.
    fn choose_abort(&mut self, rm: &'static str) {
        self.set_rm(rm, RmState::Aborted);
        tracing::info!(rm = %rm, state = %self.rm(rm).name(), "rm_choose_abort");
    }

    /// The mutant RM commits without a commit message.
    fn self_commit(&mut self, rm: &'static str) {
        self.set_rm(rm, RmState::Committed);
        tracing::info!(rm = %rm, state = %self.rm(rm).name(), "rm_rcv_commit");
    }

    fn receive(&mut self, msg: Msg, out: &mut Vec<Msg>) {
        match msg {
            Msg::Prepared(rm) => self.tm_receive_prepared(rm, out),
            Msg::Commit(rm) => {
                self.set_rm(rm, RmState::Committed);
                tracing::info!(rm = %rm, state = %self.rm(rm).name(), "rm_rcv_commit");
            }
            Msg::Abort(rm) => {
                self.set_rm(rm, RmState::Aborted);
                tracing::info!(rm = %rm, state = %self.rm(rm).name(), "rm_rcv_abort");
            }
        }
    }

    fn tm_receive_prepared(&mut self, rm: &'static str, out: &mut Vec<Msg>) {
        if self.tm != TmState::Init {
            // The decision is made; a late vote changes nothing.
            return;
        }
        if !self.prepared.contains(&rm) {
            self.prepared.push(rm);
        }
        tracing::info!(rm = %rm, prepared = self.prepared.len(), "tm_rcv_prepared");
        let enough = match self.mutant {
            Mutant::CommitOnFirstVote => !self.prepared.is_empty(),
            _ => self.prepared.len() == RMS.len(),
        };
        if enough {
            self.tm = TmState::Committed;
            tracing::info!(state = %self.tm.name(), "tm_commit");
            out.extend(RMS.iter().map(|&rm| Msg::Commit(rm)));
        }
    }

    /// The TM's timeout: it aborts a transaction that did not commit.
    fn timeout(&mut self, out: &mut Vec<Msg>) {
        self.tm = TmState::Aborted;
        tracing::info!(state = %self.tm.name(), "tm_abort");
        out.extend(RMS.iter().map(|&rm| Msg::Abort(rm)));
    }

    fn timeout_enabled(&self) -> bool {
        self.tm == TmState::Init
            || (self.mutant == Mutant::AbortAfterCommit && self.tm == TmState::Committed)
    }

    /// The actions the network can take now.
    fn actions(&self, in_flight: usize) -> Vec<Action> {
        let mut actions = Vec::new();
        for &rm in &RMS {
            match self.rm(rm) {
                RmState::Working => {
                    actions.push(Action::Prepare(rm));
                    actions.push(Action::ChooseAbort(rm));
                }
                RmState::Prepared if self.mutant == Mutant::CommitWithoutMessage => {
                    actions.push(Action::SelfCommit(rm));
                }
                _ => {}
            }
        }
        actions.extend((0..in_flight).map(Action::Deliver));
        if self.timeout_enabled() {
            actions.push(Action::Timeout);
        }
        actions
    }
}

/// The network of the two-phase commit, and the trace check of each run.
struct TwoPhaseWorkload {
    mutant: Mutant,
    verdicts: Arc<Mutex<Vec<Verdict>>>,
}

impl TwoPhaseWorkload {
    fn new(mutant: Mutant) -> (Self, Arc<Mutex<Vec<Verdict>>>) {
        let verdicts = Arc::new(Mutex::new(Vec::new()));
        let workload = Self {
            mutant,
            verdicts: Arc::clone(&verdicts),
        };
        (workload, verdicts)
    }
}

/// Pick one action. Prepares and deliveries are common, aborts and the
/// timeout are rare, so most runs commit and some abort.
fn pick(ctx: &SimContext, actions: &[Action]) -> Action {
    loop {
        let action = actions[ctx.random().random_range(0..actions.len())];
        let keep = match action {
            Action::ChooseAbort(_) => ctx.random().random_bool(0.05),
            Action::Timeout => ctx.random().random_bool(0.1),
            Action::Prepare(_) | Action::SelfCommit(_) | Action::Deliver(_) => true,
        };
        if keep {
            return action;
        }
    }
}

#[async_trait]
impl Workload for TwoPhaseWorkload {
    fn name(&self) -> &'static str {
        "two_phase"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut code = TwoPhase::new(self.mutant);
        let mut in_flight: Vec<Msg> = Vec::new();
        for _ in 0..MAX_STEPS {
            let actions = code.actions(in_flight.len());
            if actions.is_empty() {
                break;
            }
            match pick(ctx, &actions) {
                Action::Prepare(rm) => code.prepare(rm, &mut in_flight),
                Action::ChooseAbort(rm) => code.choose_abort(rm),
                Action::SelfCommit(rm) => code.self_commit(rm),
                Action::Timeout => code.timeout(&mut in_flight),
                Action::Deliver(i) => {
                    let msg = in_flight[i];
                    let roll = ctx.random().random_range(0..10_u32);
                    if roll == 0 {
                        in_flight.swap_remove(i);
                        tracing::info!(kind = %msg.kind(), rm = %msg.rm(), "msg_drop");
                    } else {
                        if roll == 1 {
                            tracing::info!(kind = %msg.kind(), rm = %msg.rm(), "msg_duplicate");
                        } else {
                            in_flight.swap_remove(i);
                        }
                        let mut sent = Vec::new();
                        code.receive(msg, &mut sent);
                        in_flight.extend(sent);
                    }
                }
            }
            let _ = ctx.time().sleep(Duration::from_millis(1)).await;
        }
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let mut trace = trace_from_timeline(ctx.observability(), &EVENTS);
        trace.define("TraceRM", Value::str_set(RMS));
        let verdict = TraceCheck::new(spec_dir(), "TwoPhaseTrace")
            .check(&trace)
            .map_err(|e| SimulationError::InvalidState(format!("trace check: {e}")))?;
        self.verdicts
            .lock()
            .expect("verdicts lock poisoned: prior task panicked")
            .push(verdict.clone());
        match verdict {
            Verdict::Accepted => Ok(()),
            Verdict::Rejected(rejection) => Err(SimulationError::InvalidState(format!(
                "TLC rejected the trace: {rejection} (event: {:?})",
                trace.events().get(rejection.index)
            ))),
            Verdict::InvariantViolated {
                invariant, matched, ..
            } => Err(SimulationError::InvalidState(format!(
                "invariant {invariant} broken after {matched} events"
            ))),
        }
    }
}

fn spec_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/tla/two_phase")
}

/// Whether TLC can run here. Without it a test is skipped, but CI must have
/// it: the nix dev shell installs `tlaplus`.
fn tlc_or_skip() -> bool {
    if TraceCheck::new(spec_dir(), "TwoPhaseTrace").tlc_available() {
        return true;
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "TLC is missing in CI: run the tests in the nix dev shell"
    );
    eprintln!("skipped: TLC (`tlc`) is not installed");
    false
}

/// Run `seeds` seeds of the two-phase commit and return each verdict.
fn run(mutant: Mutant, seeds: Vec<u64>) -> Vec<Verdict> {
    let (workload, verdicts) = TwoPhaseWorkload::new(mutant);
    let count = seeds.len();
    SimulationBuilder::new()
        .workload(workload)
        .set_iterations(count)
        .set_debug_seeds(seeds)
        .run()
        .expect("simulation configuration is valid");
    let verdicts = verdicts
        .lock()
        .expect("verdicts lock poisoned: prior task panicked")
        .clone();
    assert_eq!(verdicts.len(), count, "each seed is checked once");
    verdicts
}

/// The control: the mutant gives a plain rejection on some seed. Each
/// mutant is caught on the first of these seeds; three keep a margin.
fn assert_caught(mutant: Mutant) {
    let verdicts = run(mutant, vec![1, 2, 3]);
    assert!(
        verdicts.iter().any(|v| matches!(v, Verdict::Rejected(_))),
        "{mutant:?} must give a rejected trace on some seed: {verdicts:?}"
    );
}

#[test]
fn every_run_of_the_correct_code_is_accepted() {
    if !tlc_or_skip() {
        return;
    }
    let verdicts = run(Mutant::None, (1..=6).collect());
    assert!(
        verdicts.iter().all(Verdict::is_accepted),
        "the correct code follows the spec: {verdicts:?}"
    );
}

#[test]
fn a_commit_after_the_first_vote_is_rejected() {
    if tlc_or_skip() {
        assert_caught(Mutant::CommitOnFirstVote);
    }
}

#[test]
fn a_commit_without_the_message_is_rejected() {
    if tlc_or_skip() {
        assert_caught(Mutant::CommitWithoutMessage);
    }
}

#[test]
fn an_abort_after_the_commit_is_rejected() {
    if tlc_or_skip() {
        assert_caught(Mutant::AbortAfterCommit);
    }
}
