//! `Attrition::wipe_budget`: wipes roll through a victim pool. A wiped
//! process stays unhealed until the harness signals it healed
//! (`WipeLedger::heal`), and while the pool holds `max_unhealed` unhealed
//! processes a drawn wipe becomes a plain crash.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, Invariant, Process,
    SIM_FAULT_EVENT_NAME, SimContext, SimulationBuilder, SimulationReport, SimulationResult,
    TaskProvider, TimeProvider, TraceQuery, WipeHeal, WipeLedger, Workload, assert_always, hint,
};

/// The event a [`Node`] emits when its heal signal healed a wipe.
const HEALED: &str = "wipe_healed";

/// A server process. Each boot hints a moment every 50 ms (so the hint
/// path strikes too) and, with `heal_after`, signals itself healed that
/// long after booting.
struct Node {
    heal_after: Option<Duration>,
}

#[async_trait]
impl Process for Node {
    fn name(&self) -> &'static str {
        "node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let time = ctx.time().clone();
        let _worker = ctx.task().spawn_task("hinting worker", async move {
            loop {
                hint!("wipe budget test: a moment", 0.05).await;
                if time.sleep(Duration::from_millis(50)).await.is_err() {
                    return;
                }
            }
        });
        if let Some(heal_after) = self.heal_after {
            if ctx.time().sleep(heal_after).await.is_err() {
                return Ok(());
            }
            if WipeLedger::heal(ctx.state(), ctx.my_ip()) {
                tracing::info!(target: "wipe_test", "wipe_healed");
            }
        }
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Keeps the run alive past the chaos window.
struct Timed(Duration);

#[async_trait]
impl Workload for Timed {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(self.0).await;
        Ok(())
    }
}

/// What the invariant saw across one run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Observed {
    wipes: usize,
    crashes: usize,
    heals: usize,
    /// The most unhealed wiped processes at any wipe kill.
    max_unhealed: usize,
    /// `kind@ip@time_ms` for every kill, restart and heal, in order.
    lifecycle: Vec<String>,
}

type Shared = Arc<Mutex<Observed>>;

/// Tracks the unhealed wipes from the fault timeline and the heal events,
/// and asserts the budget at every wipe kill.
struct RollingWipes {
    max_unhealed: usize,
    fault_cursor: Cell<usize>,
    heal_cursor: Cell<usize>,
    unhealed: std::cell::RefCell<BTreeSet<String>>,
    observed: Shared,
}

impl Invariant for RollingWipes {
    fn name(&self) -> &'static str {
        "rolling_wipes"
    }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut fresh = q.since(SIM_FAULT_EVENT_NAME, &self.fault_cursor);
        fresh.extend(q.since(HEALED, &self.heal_cursor));
        fresh.sort_by_key(|event| event.seq);
        let mut observed = self.observed.lock().expect("observations lock");
        let mut unhealed = self.unhealed.borrow_mut();
        for event in fresh {
            if event.name == HEALED {
                unhealed.remove(&event.source);
                observed.heals += 1;
                observed
                    .lifecycle
                    .push(format!("healed@{}@{}", event.source, event.time_ms));
                continue;
            }
            let Some(kind @ ("process_force_kill" | "process_restart")) = event.str("kind") else {
                continue;
            };
            let ip = event.str("ip").expect("lifecycle fault carries an ip");
            let cause = event.str("cause").unwrap_or("");
            match cause {
                "crash_and_wipe" => {
                    unhealed.insert(ip.to_string());
                    observed.wipes += 1;
                    observed.max_unhealed = observed.max_unhealed.max(unhealed.len());
                    assert_always!(
                        unhealed.len() <= self.max_unhealed,
                        "wipe budget: unhealed wipes stay within max_unhealed",
                        { "unhealed" => format!("{unhealed:?}") }
                    );
                }
                "crash" => observed.crashes += 1,
                _ => {}
            }
            observed
                .lifecycle
                .push(format!("{kind}:{cause}@{ip}@{}", event.time_ms));
        }
    }

    fn reset(&mut self) {
        self.fault_cursor.set(0);
        self.heal_cursor.set(0);
        self.unhealed.borrow_mut().clear();
    }
}

/// Run `seed` with three wipe-only nodes, `max_dead: 3` (so `max_dead`
/// never bounds the wipes), and the given budget and heal delay.
fn run(seed: u64, budgeted: bool, heal_after: Option<Duration>) -> (SimulationReport, Observed) {
    run_with(seed, budgeted.then_some(WipeHeal::Signal), heal_after)
}

/// [`run`] with the budget's heal mode, `None` for no budget.
fn run_with(
    seed: u64,
    budget: Option<WipeHeal>,
    heal_after: Option<Duration>,
) -> (SimulationReport, Observed) {
    let observed: Shared = Arc::default();
    let mut config = Attrition {
        max_dead: 3,
        prob_graceful: 0.0,
        prob_crash: 0.0,
        prob_wipe: 1.0,
        recovery_delay_ms: Some(200..600),
        grace_period_ms: None,
        scope: AttritionScope::PerProcess,
        victims: AttritionVictims::group("node"),
        wipe_budget: None,
    };
    if let Some(heal) = budget {
        config = config.wipe_budget(1, heal);
    }
    let report = SimulationBuilder::new()
        .processes(3, move || Box::new(Node { heal_after }))
        .workload(Timed(Duration::from_secs(40)))
        .enable_chaos([Chaos::Attrition {
            config,
            mode: ChaosMode::Random,
        }])
        .invariant(RollingWipes {
            // An `After` bound heals without an event: only the signal
            // runs are judged here.
            max_unhealed: if budget == Some(WipeHeal::Signal) {
                1
            } else {
                usize::MAX
            },
            fault_cursor: Cell::new(0),
            heal_cursor: Cell::new(0),
            unhealed: std::cell::RefCell::default(),
            observed: observed.clone(),
        })
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .run()
        .expect("simulation configuration is valid");
    let observed = observed.lock().expect("observations lock").clone();
    (report, observed)
}

const SEEDS: [u64; 4] = [1, 7, 42, 999];

#[test]
fn a_budget_keeps_at_most_one_wipe_unhealed_and_heals_let_wipes_roll() {
    let mut downgrades = 0;
    let mut rolled = false;
    for seed in SEEDS {
        let (report, observed) = run(seed, true, Some(Duration::from_secs(3)));
        assert_eq!(
            report.failed_runs, 0,
            "seed {seed}: {:?}",
            report.assertion_violations
        );
        assert!(observed.max_unhealed <= 1, "seed {seed}: {observed:?}");
        downgrades += observed.crashes;
        // A later wipe after a heal: more wipes than one, with a heal
        // between them.
        rolled |= observed.wipes >= 2 && observed.heals >= 1;
    }
    assert!(downgrades > 0, "some drawn wipe was downgraded to a crash");
    assert!(rolled, "a heal signal let a later wipe through");
}

#[test]
fn without_a_heal_signal_a_budget_of_one_wipes_at_most_one_process() {
    for seed in SEEDS {
        let (report, observed) = run(seed, true, None);
        assert_eq!(report.failed_runs, 0, "seed {seed}");
        assert!(observed.wipes >= 1, "seed {seed}: {observed:?}");
        // Re-wiping the one unhealed process is allowed; a second
        // process never is.
        let wiped: BTreeSet<&str> = observed
            .lifecycle
            .iter()
            .filter(|entry| entry.starts_with("process_force_kill:crash_and_wipe@"))
            .filter_map(|entry| entry.split('@').nth(1))
            .collect();
        assert_eq!(wiped.len(), 1, "seed {seed}: {observed:?}");
        assert!(observed.crashes > 0, "seed {seed}: later wipes are crashes");
    }
}

#[test]
fn an_after_bound_heals_without_a_signal() {
    let mut wiped = BTreeSet::new();
    for seed in SEEDS {
        let (report, observed) =
            run_with(seed, Some(WipeHeal::After(Duration::from_secs(1))), None);
        assert_eq!(report.failed_runs, 0, "seed {seed}");
        let in_run: BTreeSet<String> = observed
            .lifecycle
            .iter()
            .filter(|entry| entry.starts_with("process_force_kill:crash_and_wipe@"))
            .filter_map(|entry| entry.split('@').nth(1).map(str::to_string))
            .collect();
        wiped.insert(in_run.len());
    }
    assert!(
        wiped.iter().any(|&count| count >= 2),
        "the bound healed a wipe, so a second process was wiped: {wiped:?}"
    );
}

#[test]
fn no_budget_keeps_wipes_bounded_by_max_dead_alone() {
    let mut piled_up = false;
    for seed in SEEDS {
        let (report, observed) = run(seed, false, Some(Duration::from_secs(3)));
        assert_eq!(report.failed_runs, 0, "seed {seed}");
        assert_eq!(observed.crashes, 0, "seed {seed}: no wipe is downgraded");
        piled_up |= observed.max_unhealed >= 2;
    }
    assert!(
        piled_up,
        "without a budget, wipes pile up past one unhealed"
    );
}

#[test]
fn a_budget_draws_nothing_the_unbudgeted_regime_does_not() {
    // With no heal and no budget, the kill and restart times (the draws)
    // must match a budgeted run's: only the wipe-or-crash cause differs.
    for seed in SEEDS {
        let (_, plain) = run(seed, false, None);
        let (_, budgeted) = run(seed, true, None);
        let times = |observed: &Observed| -> Vec<String> {
            observed
                .lifecycle
                .iter()
                .map(|entry| {
                    let (kind, rest) = entry.split_once('@').expect("kind@ip@time");
                    let kind = kind.split(':').next().unwrap_or(kind);
                    format!("{kind}@{rest}")
                })
                .collect()
        };
        assert_eq!(times(&plain), times(&budgeted), "seed {seed}");
    }
}

#[test]
fn a_budgeted_run_replays_identically_for_a_seed() {
    let (_, first) = run(7, true, Some(Duration::from_secs(3)));
    let (_, second) = run(7, true, Some(Duration::from_secs(3)));
    assert!(!first.lifecycle.is_empty());
    assert_eq!(first, second);

    let report = SimulationBuilder::new()
        .processes(3, || {
            Box::new(Node {
                heal_after: Some(Duration::from_secs(3)),
            })
        })
        .workload_factory(|| Box::new(Timed(Duration::from_secs(20))))
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead: 3,
                prob_graceful: 0.0,
                prob_crash: 0.0,
                prob_wipe: 1.0,
                recovery_delay_ms: Some(200..600),
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
                wipe_budget: None,
            }
            .wipe_budget(1, WipeHeal::Signal),
            mode: ChaosMode::Swarm,
        }])
        .chaos_duration(Duration::from_secs(15))
        .set_iterations(4)
        .check_determinism()
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report:?}");
}
