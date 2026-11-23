//! Single-decree Paxos under the walk: the proof that the crate carries a
//! real protocol, and that it bites.
//!
//! Three acceptors with a disk, two proposers. The correct protocol passes
//! every seed. A mutant whose acceptors keep their promise in memory only
//! (lost by a crash) chooses two values, and the walk finds the seed: about
//! one seed in a hundred, under a schedule with many crashes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use moonpool_tla::{Trace, TraceCheck, Value, Verdict};
use moonpool_walk::{Cx, Harness, Model, Phase, Walk, WalkError, sometimes};

const ACCEPTORS: usize = 3;
const PROPOSERS: usize = 2;
const MAJORITY: usize = ACCEPTORS / 2 + 1;

#[derive(Clone, Debug)]
enum Msg {
    Prepare {
        to: usize,
        from: usize,
        ballot: u64,
    },
    Promise {
        to: usize,
        from: usize,
        ballot: u64,
        accepted: Option<(u64, u64)>,
    },
    Accept {
        to: usize,
        ballot: u64,
        value: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Propose,
}

/// What an acceptor's disk holds, and whether the acceptor is up.
#[derive(Default)]
struct Acceptor {
    promised: u64,
    accepted: Option<(u64, u64)>,
    down: bool,
}

/// One step of the protocol, as the TLA+ spec in `tests/paxos_tla` names
/// it. A run's events are the trace that TLC checks.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    /// A proposer opened a ballot (`Phase1a`).
    Prepare { ballot: u64 },
    /// An acceptor promised a ballot and reported its accept (`Phase1b`).
    Promise {
        acceptor: usize,
        ballot: u64,
        accepted: Option<(u64, u64)>,
    },
    /// A proposer asked to accept a value (`Phase2a`).
    Propose { ballot: u64, value: u64 },
    /// An acceptor accepted a value (`Phase2b`).
    Accept {
        acceptor: usize,
        ballot: u64,
        value: u64,
    },
    /// A majority accepted the value.
    Chosen { value: u64 },
}

/// A proposer's open round.
#[derive(Default)]
struct Proposer {
    round: u64,
    ballot: u64,
    promises: BTreeMap<usize, Option<(u64, u64)>>,
    sent_accept: bool,
}

/// `MUTANT` acceptors do not write their promise to disk.
struct Paxos<const MUTANT: bool> {
    acceptors: Vec<Acceptor>,
    proposers: Vec<Proposer>,
    /// Every accept a disk ever held: `(ballot, value) -> acceptors`.
    votes: BTreeMap<(u64, u64), BTreeSet<usize>>,
    chosen: BTreeSet<u64>,
}

impl<const MUTANT: bool> Paxos<MUTANT> {
    fn propose(&mut self, proposer: usize, cx: &mut Cx<'_, Self>) {
        let state = &mut self.proposers[proposer];
        state.round += 1;
        // Ballots of two proposers never collide.
        state.ballot = state.round * PROPOSERS as u64 + proposer as u64;
        state.promises.clear();
        state.sent_accept = false;
        cx.record(Step::Prepare {
            ballot: state.ballot,
        });
        for to in 0..ACCEPTORS {
            cx.send(Msg::Prepare {
                to,
                from: proposer,
                ballot: state.ballot,
            });
        }
    }
}

impl<const MUTANT: bool> Model for Paxos<MUTANT> {
    type Msg = Msg;
    type Node = usize;
    type Action = Action;
    type Event = Step;

    fn new(_cx: &mut Cx<'_, Self>) -> Self {
        Self {
            acceptors: (0..ACCEPTORS).map(|_| Acceptor::default()).collect(),
            proposers: (0..PROPOSERS).map(|_| Proposer::default()).collect(),
            votes: BTreeMap::new(),
            chosen: BTreeSet::new(),
        }
    }

    fn actions() -> &'static [Action] {
        &[Action::Propose]
    }

    fn weight(_action: Action, _phase: Phase) -> u32 {
        8
    }

    fn act(&mut self, Action::Propose: Action, cx: &mut Cx<'_, Self>) {
        // In the quiet tail one proposer leads, so that a round can finish.
        let proposer = if cx.phase() == Phase::Chaos {
            cx.rng().index(PROPOSERS).unwrap_or(0)
        } else if self.chosen.is_empty() {
            0
        } else {
            return;
        };
        self.propose(proposer, cx);
    }

    fn deliver(&mut self, message: Msg, cx: &mut Cx<'_, Self>) {
        match message {
            Msg::Prepare { to, from, ballot } => {
                let acceptor = &mut self.acceptors[to];
                if acceptor.down || ballot <= acceptor.promised {
                    return;
                }
                acceptor.promised = ballot;
                cx.record(Step::Promise {
                    acceptor: to,
                    ballot,
                    accepted: acceptor.accepted,
                });
                cx.send(Msg::Promise {
                    to: from,
                    from: to,
                    ballot,
                    accepted: acceptor.accepted,
                });
            }
            Msg::Promise {
                to,
                from,
                ballot,
                accepted,
            } => {
                let proposer = &mut self.proposers[to];
                if ballot != proposer.ballot || proposer.sent_accept {
                    return;
                }
                proposer.promises.insert(from, accepted);
                if proposer.promises.len() < MAJORITY {
                    return;
                }
                proposer.sent_accept = true;
                let inherited = proposer.promises.values().flatten().max().map(|(_, v)| *v);
                sometimes!(
                    inherited.is_some(),
                    "a proposer adopted a value accepted at a lower ballot"
                );
                // A proposer's own value is its ballot: unique.
                let value = inherited.unwrap_or(ballot);
                cx.record(Step::Propose { ballot, value });
                for to in 0..ACCEPTORS {
                    cx.send(Msg::Accept { to, ballot, value });
                }
            }
            Msg::Accept { to, ballot, value } => {
                let acceptor = &mut self.acceptors[to];
                if acceptor.down {
                    return;
                }
                sometimes!(
                    ballot < acceptor.promised,
                    "an acceptor refused an accept below its promise"
                );
                if ballot < acceptor.promised {
                    return;
                }
                acceptor.promised = ballot;
                acceptor.accepted = Some((ballot, value));
                cx.record(Step::Accept {
                    acceptor: to,
                    ballot,
                    value,
                });
                let voters = self.votes.entry((ballot, value)).or_default();
                voters.insert(to);
                if voters.len() == MAJORITY && self.chosen.insert(value) {
                    cx.record(Step::Chosen { value });
                    sometimes!(
                        cx.phase() == Phase::Chaos,
                        "a value was chosen while faults were on"
                    );
                }
            }
        }
    }

    fn nodes(&self) -> Vec<usize> {
        (0..ACCEPTORS).collect()
    }

    fn is_up(&self, node: &usize) -> bool {
        !self.acceptors[*node].down
    }

    fn crash(&mut self, node: &usize, _cx: &mut Cx<'_, Self>) {
        let acceptor = &mut self.acceptors[*node];
        acceptor.down = true;
        if MUTANT {
            // The promise was in memory only: the disk knows the accept.
            acceptor.promised = acceptor.accepted.map_or(0, |(ballot, _)| ballot);
        }
    }

    fn restart(&mut self, node: &usize, _cx: &mut Cx<'_, Self>) {
        self.acceptors[*node].down = false;
    }

    fn check(&mut self) {
        assert!(
            self.chosen.len() <= 1,
            "at most one value is chosen: {:?}",
            self.chosen
        );
    }

    fn converged(&self) -> Result<(), String> {
        if self.chosen.is_empty() {
            Err("no value is chosen".into())
        } else {
            Ok(())
        }
    }

    fn describe(&self) -> String {
        let mut out = String::new();
        for (id, acceptor) in self.acceptors.iter().enumerate() {
            let _ = writeln!(
                out,
                "acceptor {id}: promised {} accepted {:?} down {}",
                acceptor.promised, acceptor.accepted, acceptor.down
            );
        }
        out
    }
}

#[test]
fn paxos_is_safe_under_chaos_and_live_after_it() {
    let mut chosen = 0;
    let report = Walk::<Paxos<false>>::new()
        .seeds(300)
        .chaos_steps(300)
        .env_prefix(Some("PAXOS_WALK"))
        .run_each(|run| {
            let values = run
                .events()
                .filter(|step| matches!(step, Step::Chosen { .. }));
            assert_eq!(values.count(), 1, "seed {}: one value", run.seed);
            chosen += 1;
        })
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(chosen, report.seeds);
    assert!(report.stats.crashes > 0 && report.stats.duplicated > 0);
    assert_eq!(report.sites.len(), 3, "{report}");
    assert!(report.sites.iter().all(|site| site.hits > 0), "{report}");
}

/// The schedule that hunts the mutant: the bug needs a crash between a
/// promise and an older accept, so crashes are frequent and runs are short.
/// About one seed in a hundred catches it, so 2,000 seeds cannot miss.
fn hunt() -> Walk<Paxos<true>> {
    Walk::new()
        .seeds(2_000)
        .chaos_steps(150)
        .crash_weights(20, 20)
        .deliver_weights(20, 45)
        .env_prefix(None)
}

#[test]
fn a_promise_kept_in_memory_only_is_caught() {
    let error = hunt()
        .try_run()
        .expect_err("the mutant chooses two values on some seed");
    let WalkError::Seed(failure) = error else {
        panic!("a seed fails, not the coverage: {error}");
    };
    assert!(
        failure.message.contains("at most one value is chosen"),
        "{failure}"
    );
    assert!(failure.seed < 500, "one seed in a hundred: {failure}");
    assert!(
        !failure.tail.is_empty(),
        "the failure keeps the steps that led to it"
    );
    assert!(failure.state.contains("acceptor 0"), "{failure}");

    // The seed fails again alone, at the same step: it is a regression test.
    let again = hunt()
        .replay(failure.seed)
        .err()
        .expect("the same seed fails again");
    assert_eq!(again.step, failure.step);
    assert_eq!(again.tail, failure.tail);

    // The same schedule finds nothing in the correct protocol.
    Walk::<Paxos<false>>::new()
        .seeds(300)
        .chaos_steps(150)
        .crash_weights(20, 20)
        .deliver_weights(20, 45)
        .env_prefix(None)
        .run_each(|_| {})
        .map(|_| ())
        .or_else(|error| match error {
            // Short runs with many crashes may miss a `sometimes!` site.
            WalkError::Coverage(_) => Ok(()),
            other => Err(other),
        })
        .unwrap_or_else(|error| panic!("{error}"));
}

/// The schedule the walk found for the mutant, written by hand: acceptor 1
/// promises ballot 3, crashes, and then accepts the older ballot 2.
#[test]
fn the_lost_promise_schedule_by_hand() {
    fn accepts_the_old_ballot<const MUTANT: bool>() -> bool {
        let mut harness = Harness::<Paxos<MUTANT>>::new(1);
        harness.act(Action::Propose); // may be proposer 0 or 1: only the ballot matters
        let first = harness
            .model()
            .proposers
            .iter()
            .map(|p| p.ballot)
            .max()
            .unwrap_or(0);
        // Acceptor 1 promises a newer ballot, then crashes and restarts.
        harness.deliver(Msg::Prepare {
            to: 1,
            from: 0,
            ballot: first + 10,
        });
        harness.crash(&1);
        harness.restart(&1);
        harness.deliver(Msg::Accept {
            to: 1,
            ballot: first,
            value: 7,
        });
        harness.model().acceptors[1].accepted == Some((first, 7))
    }
    assert!(!accepts_the_old_ballot::<false>(), "the promise is durable");
    assert!(accepts_the_old_ballot::<true>(), "the mutant forgot it");
}

/// The check of a walk against the TLA+ spec of Paxos in `tests/paxos_tla`.
fn spec() -> TraceCheck {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/paxos_tla");
    TraceCheck::new(dir, "PaxosTrace")
}

/// Fail the test when TLC (`tlc`, in the nix dev shell) cannot run here:
/// a missing checker is a failure, never a skip.
fn require_tlc() {
    assert!(
        spec().tlc_available(),
        "TLC (`tlc`) is missing: run the tests in the nix dev shell"
    );
}

/// A ballot, a value or an acceptor as a TLA+ integer.
fn int(number: impl TryInto<i64>) -> Value {
    Value::Int(number.try_into().unwrap_or(i64::MAX))
}

/// The whole bridge between the two crates: each event a model recorded
/// becomes one event of the trace. Neither crate knows the other.
fn trace<'a>(steps: impl Iterator<Item = &'a Step>) -> Trace {
    let mut trace = Trace::new();
    trace.define(
        "TraceAcceptor",
        Value::Set((0..ACCEPTORS).map(int).collect()),
    );
    for step in steps {
        match *step {
            Step::Prepare { ballot } => trace.event("prepare", [("bal", int(ballot))]),
            Step::Promise {
                acceptor,
                ballot,
                accepted,
            } => {
                let (at, value) = accepted.unwrap_or((0, 0));
                trace.event(
                    "promise",
                    [
                        ("acc", int(acceptor)),
                        ("bal", int(ballot)),
                        ("vbal", int(at)),
                        ("vval", int(value)),
                    ],
                );
            }
            Step::Propose { ballot, value } => {
                trace.event("propose", [("bal", int(ballot)), ("val", int(value))]);
            }
            Step::Accept {
                acceptor,
                ballot,
                value,
            } => trace.event(
                "accept",
                [
                    ("acc", int(acceptor)),
                    ("bal", int(ballot)),
                    ("val", int(value)),
                ],
            ),
            Step::Chosen { value } => trace.event("chosen", [("val", int(value))]),
        }
    }
    trace
}

/// The schedule of the TLC tests: short runs with many crashes, so that a
/// trace is a few dozen events and TLC answers in about a second.
fn short<const MUTANT: bool>() -> Walk<Paxos<MUTANT>> {
    Walk::new()
        .chaos_steps(150)
        .crash_weights(20, 20)
        .deliver_weights(20, 45)
        .env_prefix(None)
}

/// The walk checks the oracles of the model; TLC checks that every step of
/// the run is a step of the spec. The correct protocol passes both.
#[test]
fn tlc_accepts_the_walks_of_the_correct_protocol() {
    require_tlc();
    for seed in 1..=3 {
        let run = short::<false>()
            .replay(seed)
            .unwrap_or_else(|failure| panic!("{failure}"));
        let trace = trace(run.events());
        eprintln!("seed {seed}: {} events", trace.len());
        assert!(trace.len() >= 10, "seed {seed}: {} events", trace.len());
        let verdict = spec().check(&trace).expect("verdict");
        assert_eq!(verdict, Verdict::Accepted, "seed {seed}");
    }
}

/// TLC is a stricter judge than the oracle. The oracle of the model fails a
/// mutant seed only when two values are chosen, about one seed in a
/// hundred. TLC rejects a seed at the first step the spec forbids: the
/// acceptor that forgot its promise accepts, or promises, below it.
#[test]
fn tlc_rejects_a_walk_of_the_mutant_that_the_oracle_passed() {
    require_tlc();
    let mut rejected = None;
    for seed in 1..=10 {
        let Ok(run) = short::<true>().replay(seed) else {
            continue; // the oracle caught this seed itself
        };
        let steps: Vec<Step> = run.events().copied().collect();
        if let Verdict::Rejected(rejection) = spec().check(&trace(steps.iter())).expect("verdict") {
            rejected = Some((seed, steps[rejection.index], rejection.state));
            break;
        }
    }
    let (seed, step, state) = rejected.expect("a mutant seed in ten breaks the spec");
    match step {
        Step::Accept {
            acceptor, ballot, ..
        }
        | Step::Promise {
            acceptor, ballot, ..
        } => eprintln!(
            "seed {seed}: acceptor {acceptor} took ballot {ballot} below its promise\n{state}"
        ),
        other => panic!("seed {seed}: the mutant only breaks a promise: {other:?}"),
    }
}

/// The lost promise by hand, under TLC: the spec's acceptor 1 still holds
/// the promise the mutant forgot, so the accept is not a step.
#[test]
fn tlc_names_the_accept_below_a_lost_promise() {
    fn verdict<const MUTANT: bool>() -> (Verdict, Vec<Step>) {
        let mut harness = Harness::<Paxos<MUTANT>>::new(1);
        harness.with(|paxos, cx| paxos.propose(0, cx)); // ballot 2
        harness.with(|paxos, cx| paxos.propose(1, cx)); // ballot 3
        // Acceptors 0 and 2 promise ballot 2; acceptor 1 promises ballot 3,
        // then crashes and restarts.
        for (to, ballot) in [(0, 2), (2, 2), (1, 3)] {
            assert!(harness.deliver_where(
                |m| matches!(m, Msg::Prepare { to: t, ballot: b, .. } if (*t, *b) == (to, ballot))
            ));
        }
        harness.crash(&1);
        harness.restart(&1);
        // Proposer 0 has its majority and asks to accept; acceptor 1 hears it.
        while harness.deliver_where(|m| matches!(m, Msg::Promise { ballot: 2, .. })) {}
        harness.deliver_where(|m| {
            matches!(
                m,
                Msg::Accept {
                    to: 1,
                    ballot: 2,
                    ..
                }
            )
        });
        let run = harness.into_run();
        let steps: Vec<Step> = run.events().copied().collect();
        (spec().check(&trace(steps.iter())).expect("verdict"), steps)
    }
    require_tlc();
    let (correct, steps) = verdict::<false>();
    assert_eq!(correct, Verdict::Accepted, "{steps:?}");
    assert!(!steps.iter().any(|s| matches!(s, Step::Accept { .. })));

    let (mutant, steps) = verdict::<true>();
    let Verdict::Rejected(rejection) = mutant else {
        panic!("the spec forbids the accept: {mutant:?}");
    };
    assert_eq!(
        steps[rejection.index],
        Step::Accept {
            acceptor: 1,
            ballot: 2,
            value: 2
        }
    );
    assert_eq!(rejection.index, steps.len() - 1);
    assert!(rejection.state.contains("maxBal"), "{}", rejection.state);
}
