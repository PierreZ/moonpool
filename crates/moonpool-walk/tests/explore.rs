//! Coverage-driven exploration: a run that finds something new is branched,
//! so the seeds reach what plain seeds almost never reach.

use moonpool_walk::{Cx, Model, Phase, Timeline, Walk, WalkError, sometimes};

/// A ladder: a climb goes up one rung, a slip falls to the ground. Rung 8
/// needs eight climbs in a row, at one climb in four: a plain seed almost
/// never stands on it.
struct Ladder {
    rung: u32,
    /// The planted bug: the model fails on the top rung.
    breaks_at_top: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Move {
    Climb,
    Slip,
}

const TOP: u32 = 8;

impl Model for Ladder {
    type Msg = ();
    type Node = ();
    type Action = Move;
    type Event = u32;

    fn new(_cx: &mut Cx<'_, Self>) -> Self {
        Self {
            rung: 0,
            breaks_at_top: false,
        }
    }

    fn deliver(&mut self, (): (), _cx: &mut Cx<'_, Self>) {}

    fn actions() -> &'static [Move] {
        &[Move::Climb, Move::Slip]
    }

    fn weight(action: Move, phase: Phase) -> u32 {
        match (action, phase) {
            (Move::Climb, Phase::Chaos) => 1,
            (Move::Slip, Phase::Chaos) => 3,
            _ => 0,
        }
    }

    fn act(&mut self, action: Move, cx: &mut Cx<'_, Self>) {
        match action {
            Move::Climb => self.rung += 1,
            Move::Slip => self.rung = 0,
        }
        cx.record(self.rung);
        sometimes!(self.rung >= 2, "ladder: rung 2");
        sometimes!(self.rung >= 3, "ladder: rung 3");
        sometimes!(self.rung >= 4, "ladder: rung 4");
        sometimes!(self.rung >= 5, "ladder: rung 5");
        sometimes!(self.rung >= 6, "ladder: rung 6");
        sometimes!(self.rung >= 7, "ladder: rung 7");
        sometimes!(self.rung >= TOP, "ladder: rung 8");
        assert!(
            !(self.breaks_at_top && self.rung >= TOP),
            "the ladder broke at the top"
        );
    }
}

fn walk() -> Walk<Ladder> {
    Walk::<Ladder>::new()
        .env_prefix(None)
        .seeds(4)
        .chaos_steps(200)
        .quiet_steps(0)
        .deliver_weights(0, 0)
        .crash_weights(0, 0)
}

#[test]
fn plain_seeds_miss_the_top_rung() {
    let error = walk().try_run().expect_err("four plain seeds miss rung 8");
    let WalkError::Coverage(gaps) = error else {
        panic!("a coverage gap, not {error}");
    };
    assert!(gaps.iter().any(|gap| gap.contains("rung 8")), "{gaps:?}");
}

#[test]
fn an_exploring_campaign_reaches_the_top_rung() {
    let report = walk()
        .explore(300)
        .try_run()
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(report.seeds, 4);
    assert!(report.branched > 0 && report.discoveries > 0, "{report}");
    assert!(report.timelines <= 4 + 4 * 300, "{report}");
    assert!(report.to_string().contains("explored:"), "{report}");
}

#[test]
fn a_timeline_replays_exactly() {
    let mut branched = Vec::new();
    walk()
        .explore(40)
        .run_each(|run| {
            if !run.timeline.branches.is_empty() {
                branched.push((
                    run.timeline.clone(),
                    run.events().copied().collect::<Vec<_>>(),
                ));
            }
        })
        .ok();
    assert!(!branched.is_empty(), "the campaign branched");
    for (timeline, events) in branched {
        let again = walk()
            .replay_timeline(&timeline)
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert_eq!(again.timeline, timeline);
        assert_eq!(
            again.events().copied().collect::<Vec<_>>(),
            events,
            "{timeline}"
        );
        // A child shares its parent's steps up to the branch point.
        let parent = Timeline {
            seed: timeline.seed,
            branches: timeline.branches[..timeline.branches.len() - 1].to_vec(),
        };
        let parent = walk().replay_timeline(&parent).expect("the parent passes");
        let shared = usize::try_from(timeline.branches.last().expect("a branch").step).unwrap_or(0);
        let prefix = |run: &moonpool_walk::Run<Ladder>| {
            run.log
                .iter()
                .filter(|logged| logged.step <= u64::try_from(shared).unwrap_or(0))
                .map(|logged| format!("{logged:?}"))
                .collect::<Vec<_>>()
        };
        assert_eq!(prefix(&parent), prefix(&again), "{timeline}");
    }
}

#[test]
fn a_failure_in_a_child_names_its_timeline() {
    let error = walk()
        .model(|_| Ladder {
            rung: 0,
            breaks_at_top: true,
        })
        .explore(300)
        .env_prefix(Some("LADDER"))
        .try_run()
        .expect_err("the top rung breaks");
    let WalkError::Seed(failure) = error else {
        panic!("a seed failure, not {error}");
    };
    assert!(!failure.timeline.branches.is_empty(), "{failure}");
    let replay = failure.replay.clone().expect("a replay hint");
    assert_eq!(replay, format!("LADDER_TIMELINE={}", failure.timeline));
    assert!(failure.to_string().contains("timeline "), "{failure}");
    let again = walk()
        .model(|_| Ladder {
            rung: 0,
            breaks_at_top: true,
        })
        .replay_timeline(&failure.timeline)
        .err()
        .expect("the timeline fails again");
    assert_eq!(again.step, failure.step);
}

#[test]
fn an_edge_count_that_grows_is_a_discovery() {
    use std::cell::Cell;
    thread_local! {
        static EDGES: Cell<usize> = const { Cell::new(0) };
    }
    fn edges() -> usize {
        // A count that grows on every read: every step is new.
        EDGES.with(|edges| {
            edges.set(edges.get() + 1);
            edges.get()
        })
    }
    let mut timelines = 0;
    let _ = Walk::<Ladder>::new()
        .env_prefix(None)
        .seeds(1)
        .chaos_steps(10)
        .quiet_steps(0)
        .deliver_weights(0, 0)
        .crash_weights(0, 0)
        .edge_coverage(edges)
        .explore(5)
        .branching(1)
        .run_each(|_| timelines += 1);
    assert_eq!(timelines, 6, "the root and five children");
}
