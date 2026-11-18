//! What the walk promises, on a small gossip model with one flaw for each
//! promise.

use std::sync::atomic::{AtomicU64, Ordering};

use moonpool_assertions::{AssertKind, assertion_bool};
use moonpool_walk::{
    Cx, Entry, Harness, Model, Phase, Walk, WalkError, buggify, buggify_fault_with_prob, reachable,
    sometimes,
};

const NODES: usize = 4;

const GOOD: u8 = 0;
const NEVER_CONVERGES: u8 = 1;
const NEVER_REACHED: u8 = 2;
const RECORDS_A_FAILURE: u8 = 3;
const READS_A_GLOBAL: u8 = 4;
const PANICS_AT_BIRTH: u8 = 5;
const ONE_STAYS_DEAD: u8 = 6;
const CRASHES_BY_ITSELF: u8 = 7;
const DECLARES_A_SITE: u8 = 8;

const FINISHED: &str = "a run finished";
const NEVER: &str = "a site that the model declares and never reaches";
/// The node that the `ONE_STAYS_DEAD` flaw keeps down for the tail.
const DEAD: usize = 3;

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
    /// The tail started: node `DEAD` refuses to restart (`ONE_STAYS_DEAD`).
    dead_for_good: bool,
    /// How many times a restart was refused.
    refused: u64,
}

impl<const FLAW: u8> Gossip<FLAW> {
    fn fresh(highest: u64) -> Self {
        Self {
            values: vec![0; NODES],
            down: vec![false; NODES],
            highest,
            dead_for_good: false,
            refused: 0,
        }
    }
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
        Self::fresh(1)
    }

    fn actions() -> &'static [Action] {
        &[Action::Bump, Action::Gossip]
    }

    fn weight(action: Action, phase: Phase) -> u32 {
        match (action, phase) {
            (Action::Bump, Phase::Quiet | Phase::Settle) => 0,
            _ => 10,
        }
    }

    fn act(&mut self, action: Action, cx: &mut Cx<'_, Self>) {
        match action {
            Action::Bump => {
                let Some(to) = cx.rng().index(NODES) else {
                    return;
                };
                self.highest += 1;
                if FLAW == READS_A_GLOBAL {
                    self.highest += GLOBAL.fetch_add(1, Ordering::Relaxed);
                }
                cx.send((to, self.highest));
            }
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
        if buggify_fault_with_prob!(0.2) {
            assert!(
                cx.phase() == Phase::Chaos,
                "a fault site is silent in the tail"
            );
            sometimes!(true, "a fault site fired in the model");
            return;
        }
        if FLAW == CRASHES_BY_ITSELF && cx.phase() == Phase::Chaos && cx.rng().chance(1, 20) {
            // Between the write and its sync, say.
            self.down[to] = true;
            cx.crashed(&to);
            return;
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
        if self.dead_for_good && *node == DEAD {
            self.refused += 1;
            return;
        }
        self.down[*node] = false;
    }

    fn calm(&mut self, _cx: &mut Cx<'_, Self>) {
        if FLAW == ONE_STAYS_DEAD {
            self.dead_for_good = true;
            self.down[DEAD] = true;
        }
    }

    fn finish(&mut self) {
        reachable!(FINISHED);
    }

    fn reachable_sites() -> &'static [&'static str] {
        if FLAW == DECLARES_A_SITE {
            &[FINISHED, NEVER]
        } else {
            &[FINISHED]
        }
    }

    fn check(&mut self) {
        assert!(self.values.iter().all(|v| *v <= self.highest));
    }

    fn converged(&self) -> Result<(), String> {
        // The highest value may be lost with its only message. The nodes
        // must still agree.
        if FLAW == NEVER_CONVERGES {
            Err("this model never converges".into())
        } else if (0..NODES)
            .filter(|node| !self.down[*node])
            .all(|node| self.values[node] == self.values[0])
        {
            Ok(())
        } else {
            Err(format!("the nodes disagree: {:?}", self.values))
        }
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
        other => panic!("a seed fails, nothing else: {other}"),
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
    for wanted in ["a buggify site fired", "a fault site fired", FINISHED] {
        assert!(
            report
                .sites
                .iter()
                .any(|site| site.hits > 0 && site.message.contains(wanted)),
            "{wanted}: {report}"
        );
    }
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
    assert_eq!(run.model.converged(), Ok(()));
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
    assert!(
        failure.message.ends_with("this model never converges"),
        "{failure}"
    );
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
    // The seed stops at the step that records the failure, not at its end.
    assert!(failure.step < 150, "{failure}");
    let step = format!("{:>6} ", failure.step);
    assert!(
        failure.tail.iter().any(|line| {
            line.starts_with(&step) && line.contains("deliver") && line.contains(", 4)")
        }),
        "the failing delivery is in the failing step: {failure}"
    );
    let again = walk::<RECORDS_A_FAILURE>()
        .replay(failure.seed)
        .err()
        .expect("it fails again");
    assert_eq!(again.step, failure.step);
    walk::<RECORDS_A_FAILURE>()
        .check_determinism(failure.seed)
        .expect("a failing seed replays too");
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
    assert_eq!(report.sites.len(), 1, "buggify was left alone: {report}");
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
        .deliver_weights(80, 80)
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
    let mut harness = Harness::<Gossip<GOOD>>::build(1, 16, |_cx| Gossip::fresh(9));
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

#[test]
fn a_site_that_the_model_declares_must_be_reached() {
    let error = walk::<DECLARES_A_SITE>()
        .try_run()
        .expect_err("a declared site is a gap");
    let WalkError::Coverage(gaps) = &error else {
        panic!("every seed passes: {error}");
    };
    assert_eq!(gaps.len(), 1, "{error}");
    assert!(gaps[0].contains(NEVER), "{error}");
}

#[test]
fn a_node_may_refuse_to_restart_for_the_tail() {
    walk::<ONE_STAYS_DEAD>()
        .run_each(|run| {
            assert!(
                run.model.down[DEAD] && run.model.refused > 50,
                "asked at every step"
            );
            let calm = run
                .log
                .iter()
                .position(|logged| matches!(logged.entry, Entry::Calm))
                .expect("the faults stop once");
            assert!(
                !run.log[calm..]
                    .iter()
                    .any(|logged| matches!(logged.entry, Entry::Restart(DEAD))),
                "a refused restart is not a restart"
            );
            let came_up = run
                .log
                .iter()
                .filter(|logged| matches!(logged.entry, Entry::Restart(_)))
                .count() as u64;
            assert_eq!(run.stats.restarts, came_up, "seed {}", run.seed);
        })
        .unwrap_or_else(|error| panic!("{error}"));
}

#[test]
fn a_node_that_crashes_by_itself_is_in_the_log() {
    let report = walk::<CRASHES_BY_ITSELF>()
        .crash_weights(0, 3)
        .run_each(|run| {
            let logged = run
                .log
                .iter()
                .filter(|logged| matches!(logged.entry, Entry::Crash(_)))
                .count() as u64;
            assert_eq!(run.stats.crashes, logged);
        })
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        report.stats.crashes > 0,
        "the walk made none: the model did"
    );
    assert!(report.stats.restarts > 0);
}

#[test]
fn a_walk_inside_a_simulation_leaves_buggify_alone() {
    moonpool_buggify::set_random_source(|| 0.99);
    moonpool_buggify::buggify_init(0.5);
    assert!(moonpool_buggify::is_simulated());
    // No buggify site of the model fires on a source that never does, so
    // the campaign has coverage gaps: only its effect on buggify matters.
    let _ = walk::<GOOD>().seeds(2).try_run();
    assert!(
        moonpool_buggify::is_simulated(),
        "the simulation still runs"
    );
    assert!(
        !moonpool_buggify::buggify_is_recovering(),
        "its fault sites still fire"
    );
    moonpool_buggify::buggify_reset();
    moonpool_buggify::clear_random_source();

    // Outside a simulation, a walk told to leave buggify alone does so.
    walk::<GOOD>().seeds(1).buggify(None).run();
    assert!(!moonpool_buggify::buggify_is_recovering());
}

#[test]
fn a_walk_in_a_table_that_is_not_its_own_judges_no_coverage() {
    moonpool_assertions::init();
    sometimes!(false, "a site of the enclosing run");
    assertion_bool(
        AssertKind::Always,
        true,
        false,
        "a failure of the enclosing run",
    );

    let report = walk::<GOOD>()
        .seeds(3)
        .expect_sometimes("declared by the walk")
        .report();
    assert_eq!(report.seeds, 3);
    assert!(
        report.sites.is_empty(),
        "the owner of the table reports: {report}"
    );

    let slots = moonpool_assertions::assertion_read_all();
    assert!(
        slots.iter().any(|slot| slot.msg == FINISHED),
        "the walk counted in the table"
    );
    assert!(
        !slots.iter().any(|slot| slot.msg == "declared by the walk"),
        "the walk declared nothing in it"
    );
    moonpool_assertions::clear();
}

#[test]
fn a_failure_keeps_as_much_of_the_log_as_asked() {
    let tail = |entries| {
        seed_failure(
            walk::<NEVER_CONVERGES>()
                .settle_rounds(1)
                .failure_tail(entries)
                .try_run()
                .expect_err("the liveness claim does not hold"),
        )
        .tail
    };
    assert_eq!(tail(5).len(), 5);
    let whole = tail(usize::MAX);
    assert!(
        whole.len() > 200 && whole[0].contains("chaos"),
        "the whole log"
    );
}

#[test]
fn a_factory_builds_the_model_of_each_seed() {
    let report = walk::<GOOD>()
        .seeds(5)
        .model(|cx| {
            cx.send((1, 50));
            Gossip::fresh(50)
        })
        .run_each(|run| assert!(run.model.highest >= 50))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(report.seeds, 5);
}

#[test]
fn a_script_ends_with_the_tail_of_a_walk() {
    let tail = walk::<GOOD>().quiet_steps(30);
    let mut harness = Harness::<Gossip<GOOD>>::new(4);
    harness.crash(&2);
    harness.with(|model, cx| {
        model.highest = 8;
        cx.send((1, 8));
    });
    harness.mailbox_mut().retain(|(to, _)| *to == 1);
    assert_eq!(harness.mailbox().len(), 1);
    assert!(harness.deliver_next());
    assert!(
        harness.model().converged().is_err(),
        "only node 1 heard of 8"
    );

    tail.finish(&mut harness).expect("the tail converges");
    assert_eq!(harness.phase(), Phase::Settle);
    assert_eq!(harness.model().values, [8; NODES]);

    let mut stuck = Harness::<Gossip<NEVER_CONVERGES>>::new(4);
    let error = walk::<NEVER_CONVERGES>()
        .settle_rounds(2)
        .finish(&mut stuck)
        .expect_err("no tail helps this model");
    assert!(error.contains("2 settle rounds"), "{error}");
}

#[test]
fn a_move_reads_its_step_in_the_phase() {
    /// A model whose only move checks the clock of its phase.
    struct Clock {
        last: Option<(Phase, u64)>,
    }
    impl Model for Clock {
        type Msg = ();
        type Node = ();
        type Action = ();
        type Event = ();
        fn new(_cx: &mut Cx<'_, Self>) -> Self {
            Self { last: None }
        }
        fn actions() -> &'static [()] {
            &[()]
        }
        fn weight((): (), _phase: Phase) -> u32 {
            50
        }
        fn act(&mut self, (): (), cx: &mut Cx<'_, Self>) {
            let now = (cx.phase(), cx.phase_step());
            assert!(cx.phase_step() <= cx.step());
            if let Some((phase, step)) = self.last
                && phase == now.0
            {
                assert!(step < now.1, "the step of a phase only grows");
            }
            if cx.phase() != Phase::Chaos {
                assert!(cx.phase_step() < cx.step(), "a later phase starts later");
            }
            self.last = Some(now);
        }
        fn deliver(&mut self, (): (), _cx: &mut Cx<'_, Self>) {}
    }
    let run = Walk::<Clock>::new()
        .env_prefix(None)
        .chaos_steps(50)
        .quiet_steps(50)
        .replay(1)
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert_eq!(run.model.last.map(|(phase, _)| phase), Some(Phase::Quiet));
}
