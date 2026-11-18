//! What the walk promises, on a small gossip model with one flaw for each
//! promise.

use std::sync::atomic::{AtomicU64, Ordering};

use moonpool_assertions::{AssertKind, assertion_bool};
use moonpool_walk::{Cx, Entry, Harness, Model, Phase, Walk, WalkError, buggify, sometimes};

const NODES: usize = 4;

const GOOD: u8 = 0;
const NEVER_CONVERGES: u8 = 1;
const NEVER_REACHED: u8 = 2;
const RECORDS_A_FAILURE: u8 = 3;
const READS_A_GLOBAL: u8 = 4;
const PANICS_AT_BIRTH: u8 = 5;

/// Read by the `READS_A_GLOBAL` flaw: state that outlives a run.
static GLOBAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Bump,
    Gossip,
}

/// Every node keeps the highest value it heard of. `FLAW` breaks one
/// promise of the model.
struct Gossip<const FLAW: u8> {
    /// On each node's disk.
    values: Vec<u64>,
    down: Vec<bool>,
    highest: u64,
}

impl<const FLAW: u8> Model for Gossip<FLAW> {
    /// `(to, value)`.
    type Msg = (usize, u64);
    type Node = usize;
    type Action = Action;
    /// A node learned a higher value: `(node, value)`.
    type Event = (usize, u64);

    fn new(cx: &mut Cx<'_, Self>) -> Self {
        assert!(FLAW != PANICS_AT_BIRTH, "the model cannot be built");
        cx.send((0, 1));
        Self {
            values: vec![0; NODES],
            down: vec![false; NODES],
            highest: 1,
        }
    }

    fn actions() -> &'static [(Action, u32)] {
        &[(Action::Bump, 10), (Action::Gossip, 10)]
    }

    fn act(&mut self, action: Action, cx: &mut Cx<'_, Self>) {
        match action {
            Action::Bump if cx.phase() == Phase::Chaos => {
                let Some(to) = cx.rng().index(NODES) else {
                    return;
                };
                self.highest += 1;
                if FLAW == READS_A_GLOBAL {
                    self.highest += GLOBAL.fetch_add(1, Ordering::Relaxed);
                }
                cx.send((to, self.highest));
            }
            Action::Bump => {}
            Action::Gossip => {
                for from in 0..NODES {
                    if self.down[from] {
                        continue;
                    }
                    for to in 0..NODES {
                        cx.send((to, self.values[from]));
                    }
                }
            }
        }
    }

    fn deliver(&mut self, (to, value): (usize, u64), cx: &mut Cx<'_, Self>) {
        if self.down[to] {
            return;
        }
        if buggify!() {
            sometimes!(true, "a buggify site fired in the model");
            return; // a slow node misses the message
        }
        if FLAW == NEVER_REACHED {
            sometimes!(value == u64::MAX, "a node heard the largest value there is");
        }
        if FLAW == RECORDS_A_FAILURE {
            assertion_bool(
                AssertKind::Always,
                true,
                value < 4,
                "values stay below four",
            );
        }
        if value > self.values[to] {
            self.values[to] = value;
            cx.record((to, value));
        }
    }

    fn nodes(&self) -> Vec<usize> {
        (0..NODES).collect()
    }

    fn is_up(&self, node: &usize) -> bool {
        !self.down[*node]
    }

    fn crash(&mut self, node: &usize, _cx: &mut Cx<'_, Self>) {
        self.down[*node] = true;
    }

    fn restart(&mut self, node: &usize, _cx: &mut Cx<'_, Self>) {
        self.down[*node] = false;
    }

    fn check(&mut self) {
        assert!(self.values.iter().all(|v| *v <= self.highest));
    }

    fn converged(&self) -> bool {
        // The highest value may be lost with its only message. The nodes
        // must still agree.
        FLAW != NEVER_CONVERGES && self.values.iter().all(|v| *v == self.values[0])
    }

    fn describe(&self) -> String {
        format!("values {:?} down {:?}", self.values, self.down)
    }
}

fn walk<const FLAW: u8>() -> Walk<Gossip<FLAW>> {
    Walk::new()
        .seeds(40)
        .chaos_steps(150)
        .quiet_steps(50)
        .env_prefix(None)
}

fn seed_failure(error: WalkError) -> moonpool_walk::SeedFailure {
    match error {
        WalkError::Seed(failure) => *failure,
        WalkError::Coverage(gaps) => panic!("a seed fails, not the coverage: {gaps:?}"),
    }
}

#[test]
fn a_campaign_counts_what_it_did() {
    let report = walk::<GOOD>().report();
    assert_eq!(report.seeds, 40);
    assert_eq!(report.actions, ["Bump", "Gossip"]);
    let stats = &report.stats;
    assert!(stats.steps >= 40 * 200, "chaos and quiet steps: {stats:?}");
    assert!(stats.delivered > 0 && stats.dropped > 0 && stats.duplicated > 0);
    assert!(stats.crashes > 0 && stats.restarts > 0 && stats.events > 0);
    assert!(stats.actions.iter().all(|count| *count > 0));
    assert!(
        report
            .sites
            .iter()
            .any(|site| site.hits > 0 && site.message.contains("buggify")),
        "the walk seeds buggify: {report}"
    );
    assert!(report.to_string().contains("40 seeds"));
    walk::<GOOD>().run();
}

#[test]
fn a_run_keeps_its_log_and_its_events() {
    let run = walk::<GOOD>()
        .replay(7)
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert_eq!(run.seed, 7);
    assert_eq!(run.events().count() as u64, run.stats.events);
    assert!(run.model.converged());
    let calm = run
        .log
        .iter()
        .position(|logged| matches!(logged.entry, Entry::Calm))
        .expect("the faults stop once");
    assert!(
        run.log[..calm]
            .iter()
            .all(|logged| logged.phase == Phase::Chaos)
    );
    assert!(
        run.log[calm..].iter().all(|logged| {
            logged.phase != Phase::Chaos
                && !matches!(
                    logged.entry,
                    Entry::Drop(_) | Entry::Duplicate(_) | Entry::Crash(_)
                )
        }),
        "no fault in the tail"
    );
    assert!(run.log.windows(2).all(|pair| pair[0].step <= pair[1].step));
}

#[test]
fn a_seed_replays() {
    walk::<GOOD>()
        .check_determinism(3)
        .expect("the model draws from the seed only");
    let error = walk::<READS_A_GLOBAL>()
        .check_determinism(3)
        .expect_err("the model reads state that outlives the run");
    assert!(error.contains("not deterministic"), "{error}");
}

#[test]
fn a_tail_that_does_not_converge_fails_the_seed() {
    let failure = seed_failure(
        walk::<NEVER_CONVERGES>()
            .settle_rounds(3)
            .try_run()
            .expect_err("the liveness claim does not hold"),
    );
    assert_eq!(failure.seed, 1);
    assert!(failure.message.contains("did not converge"), "{failure}");
    assert!(failure.state.starts_with("values"), "{failure}");
    assert_eq!(failure.replay, None, "this walk reads no environment");
}

#[test]
fn a_site_that_no_seed_reaches_fails_the_campaign() {
    let error = walk::<NEVER_REACHED>()
        .try_run()
        .expect_err("a coverage gap");
    let WalkError::Coverage(gaps) = &error else {
        panic!("every seed passes: {error}");
    };
    assert_eq!(gaps.len(), 1);
    assert!(
        gaps[0].contains("a node heard the largest value"),
        "{error}"
    );

    // A site that is never evaluated is invisible, unless it is declared.
    let declared = "the model has no such site";
    let error = walk::<GOOD>()
        .expect_sometimes(declared)
        .try_run()
        .expect_err("a declared site is a gap");
    assert!(error.to_string().contains(declared), "{error}");
    let error = walk::<GOOD>()
        .expect_reachable(declared)
        .try_run()
        .expect_err("a declared site is a gap");
    assert!(error.to_string().contains(declared), "{error}");

    // One seed alone proves nothing about coverage, and is not judged.
    assert!(walk::<NEVER_REACHED>().replay(1).is_ok());
}

#[test]
fn an_always_assertion_that_records_a_failure_fails_the_seed() {
    let failure = seed_failure(
        walk::<RECORDS_A_FAILURE>()
            .try_run()
            .expect_err("the table holds a failure"),
    );
    assert!(
        failure.message.contains("values stay below four"),
        "{failure}"
    );
    assert!(walk::<RECORDS_A_FAILURE>().replay(failure.seed).is_err());
}

#[test]
fn a_panic_in_the_model_is_the_failure_of_the_seed() {
    let failure = seed_failure(walk::<PANICS_AT_BIRTH>().try_run().expect_err("no model"));
    assert_eq!(failure.step, 0);
    assert!(failure.message.contains("cannot be built"), "{failure}");
    assert!(failure.tail.is_empty());
}

#[test]
fn a_swarm_switches_families_off_for_a_seed() {
    let mut quiet_networks = 0;
    let mut no_crash = 0;
    let mut one_action = 0;
    let report = walk::<GOOD>()
        .swarm(true)
        .buggify(None)
        .run_each(|run| {
            quiet_networks += u64::from(run.stats.dropped == 0);
            no_crash += u64::from(run.stats.crashes == 0);
            let chaos_acts = |wanted: Action| {
                run.log.iter().any(|logged| {
                    logged.phase == Phase::Chaos
                        && matches!(logged.entry, Entry::Act(action) if action == wanted)
                })
            };
            assert!(
                chaos_acts(Action::Bump) || chaos_acts(Action::Gossip),
                "a seed keeps one action"
            );
            one_action += u64::from(chaos_acts(Action::Bump) != chaos_acts(Action::Gossip));
        })
        .unwrap_or_else(|error| panic!("{error}"));
    for count in [quiet_networks, no_crash, one_action] {
        assert!(
            count > 0 && count < report.seeds,
            "some seeds, not all: {count}"
        );
    }
    assert!(report.sites.is_empty(), "buggify was left alone: {report}");
}

#[test]
fn the_walk_keeps_at_most_max_down_nodes_down() {
    walk::<GOOD>()
        .max_down(1)
        .crash_weights(30, 1)
        .run_each(|run| {
            let mut down = 0_i32;
            for logged in &run.log {
                match logged.entry {
                    Entry::Crash(_) => down += 1,
                    Entry::Restart(_) => down -= 1,
                    _ => {}
                }
                assert!(down <= 1, "seed {}: two nodes down", run.seed);
            }
        })
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn the_knobs_of_the_schedule_are_honoured() {
    let report = walk::<GOOD>()
        .first_seed(900)
        .seeds(3)
        .drop_probability(0.0)
        .duplicate_probability(0.0)
        .crash_weights(0, 0)
        .deliver_weight(80)
        .deliveries_per_step(1, 2)
        .mailbox(4)
        .drain_steps(100)
        .report();
    assert_eq!(report.seeds, 3);
    let stats = &report.stats;
    assert_eq!((stats.dropped, stats.duplicated, stats.crashes), (0, 0, 0));
    assert!(stats.evicted > 0, "a mailbox of four overflows: {stats:?}");
}

#[test]
fn a_harness_scripts_a_schedule() {
    let mut harness = Harness::<Gossip<GOOD>>::build(1, 16, |_cx| Gossip {
        values: vec![0; NODES],
        down: vec![false; NODES],
        highest: 9,
    });
    assert!(
        harness.mailbox().is_empty(),
        "this model sent nothing at birth"
    );
    harness.send((0, 5));
    harness.send((1, 6));
    harness.send((2, 7));
    assert_eq!(harness.drop_where(|(to, _)| *to == 1), 1);
    assert!(harness.deliver_where(|(to, _)| *to == 2));
    harness.crash(&0);
    assert!(
        harness.deliver_random(),
        "node 0 is down: the message is lost"
    );
    assert!(!harness.deliver_next(), "nothing is in flight");
    assert_eq!(harness.model().values, [0, 0, 7, 0]);

    assert_eq!(harness.phase(), Phase::Chaos);
    harness.calm();
    assert_eq!(harness.phase(), Phase::Quiet);
    assert!(harness.model().is_up(&0), "calm restarts what is down");
    harness.crash(&3);
    harness.restart(&3);
    harness.act(Action::Gossip);
    assert_eq!(harness.mailbox().len(), 16);
    assert!(harness.stats().evicted == 0 && harness.drain(1_000) == 16);
    harness.model_mut().highest = 10;
    let draw = harness.rng().below(10);
    harness.deliver((3, draw));

    let stats = harness.stats().clone();
    assert_eq!((stats.delivered, stats.dropped), (19, 1));
    assert_eq!((stats.crashes, stats.restarts), (2, 2));
    assert_eq!(stats.actions, [0, 1]);
    assert_eq!(harness.log().len(), harness.into_run().log.len());
}
