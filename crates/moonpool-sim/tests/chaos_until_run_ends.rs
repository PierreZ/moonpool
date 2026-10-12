//! `SimulationBuilder::chaos_until_run_ends`: chaos lasts until the last
//! workload's `run()` returns (the duration is a cap), an early `run()` does
//! not shut its siblings down, and `check()` runs on live processes after
//! the cutoff.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Process, SimContext, SimulationBuilder, SimulationError, SimulationResult, TimeProvider,
    Workload, chaos_ended_at, in_chaos,
};

/// What the run saw, shared with the factories.
#[derive(Default)]
struct Seen {
    /// Every heartbeat time of the process.
    beats: Vec<Duration>,
    /// Per `run()` end: whether chaos was still on, and whether the
    /// workload had been told to shut down.
    run_ends: Vec<(bool, bool)>,
    /// The cutoff each `check()` read.
    cutoffs: Vec<Option<Duration>>,
    /// Whether each `check()` saw a heartbeat after the cutoff.
    live_in_check: Vec<bool>,
}

type Shared = Arc<Mutex<Seen>>;

fn lock(seen: &Shared) -> std::sync::MutexGuard<'_, Seen> {
    seen.lock().expect("Mutex poisoned: prior task panicked")
}

/// A process that beats every 50 ms until it is shut down.
struct Beat(Shared);

#[async_trait]
impl Process for Beat {
    fn name(&self) -> &'static str {
        "beat"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        while !ctx.shutdown().is_cancelled() {
            lock(&self.0).beats.push(ctx.time().now());
            if ctx.time().sleep(Duration::from_millis(50)).await.is_err() {
                break;
            }
        }
        Ok(())
    }
}

/// A workload whose `run()` lasts `run_for`, and whose `check()` waits for
/// a heartbeat after the cutoff.
struct Timed {
    run_for: Duration,
    seen: Shared,
}

#[async_trait]
impl Workload for Timed {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(self.run_for)
            .await
            .map_err(|e| SimulationError::InvalidState(e.to_string()))?;
        let ends = (in_chaos(), ctx.shutdown().is_cancelled());
        lock(&self.seen).run_ends.push(ends);
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let cutoff = chaos_ended_at();
        ctx.time()
            .sleep(Duration::from_millis(500))
            .await
            .map_err(|e| SimulationError::InvalidState(e.to_string()))?;
        let mut seen = lock(&self.seen);
        let live = cutoff.is_some_and(|at| seen.beats.iter().any(|beat| *beat > at));
        seen.cutoffs.push(cutoff);
        seen.live_in_check.push(live);
        Ok(())
    }
}

fn run(cap: Duration, seen: &Shared) {
    let (a, b, p) = (seen.clone(), seen.clone(), seen.clone());
    let report = SimulationBuilder::new()
        .processes(1, move || Box::new(Beat(p.clone())))
        .workload(Timed {
            run_for: Duration::from_secs(1),
            seen: a,
        })
        .workload(Timed {
            run_for: Duration::from_secs(3),
            seen: b,
        })
        .chaos_duration(cap)
        .chaos_until_run_ends()
        .set_debug_seeds(vec![7])
        .set_iterations(1)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "the run is green");
}

#[test]
fn chaos_lasts_until_the_last_run_returns_and_check_meets_live_processes() {
    let seen = Shared::default();
    run(Duration::from_mins(10), &seen);
    let seen = lock(&seen);
    assert_eq!(
        seen.run_ends,
        vec![(true, false), (true, false)],
        "both runs end inside chaos, and the early one shuts no sibling down"
    );
    for cutoff in &seen.cutoffs {
        let at = cutoff.expect("check reads the cutoff");
        assert!(
            at >= Duration::from_secs(3) && at < Duration::from_secs(4),
            "chaos ends when the last run returns: {at:?}"
        );
    }
    assert_eq!(
        seen.live_in_check,
        vec![true, true],
        "check meets the live process"
    );
}

#[test]
fn the_duration_caps_the_chaos_phase() {
    let seen = Shared::default();
    run(Duration::from_secs(2), &seen);
    let seen = lock(&seen);
    assert_eq!(
        seen.run_ends,
        vec![(true, false), (false, false)],
        "the long run ends after the cap"
    );
    for cutoff in &seen.cutoffs {
        let at = cutoff.expect("check reads the cutoff");
        assert!(
            at >= Duration::from_secs(2) && at < Duration::from_secs(3),
            "the cap ends chaos: {at:?}"
        );
    }
}

#[test]
fn chaos_until_run_ends_needs_a_duration() {
    let result = SimulationBuilder::new()
        .workload(Timed {
            run_for: Duration::from_millis(1),
            seen: Shared::default(),
        })
        .chaos_until_run_ends()
        .set_iterations(1)
        .run();
    assert!(
        matches!(result, Err(SimulationError::InvalidConfiguration(ref m)) if m.contains("chaos_until_run_ends")),
        "refused: {:?}",
        result.err()
    );
}
