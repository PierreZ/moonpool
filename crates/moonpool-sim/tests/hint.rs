//! `moonpool_buggify::hint!`: production code names a moment, and the
//! simulator reboots the process there when the seed's attrition regime
//! allows it. The point fires from a task the process spawned, so the
//! executor's task owner carries the process across spawns.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, HintVeto, Process, SimContext,
    SimulationBuilder, SimulationError, SimulationResult, TaskProvider, TimeProvider, Workload,
    hint,
};

/// Spawns a worker that reaches one hinted moment every 10ms. The worker
/// marks the moment armed around the hint, so the next boot knows the last
/// one died exactly there: the hint resolves without yielding, and any
/// other kill lands at the sleep, where the mark is cleared.
struct Hinter {
    killed_at_hint: Arc<AtomicU64>,
    /// When set, the harness's veto: every reboot it is asked about is
    /// counted here and refused.
    vetoed: Option<Arc<AtomicU64>>,
}

#[async_trait]
impl Process for Hinter {
    fn name(&self) -> &'static str {
        "hinter"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let armed = format!("armed:{}", ctx.my_ip());
        if ctx.state().get::<bool>(&armed).unwrap_or(false) {
            self.killed_at_hint.fetch_add(1, Ordering::Relaxed);
        }
        ctx.state().publish(&armed, false);
        if let Some(vetoed) = &self.vetoed {
            let vetoed = Arc::clone(vetoed);
            HintVeto::new(move |_ip, label| {
                assert_eq!(label, "test: a moment worth a reboot");
                vetoed.fetch_add(1, Ordering::Relaxed);
                false
            })
            .publish(ctx.state());
        }
        let state = ctx.state().clone();
        let time = ctx.time().clone();
        let _worker = ctx.task().spawn_task("hinting worker", async move {
            loop {
                state.publish(&armed, true);
                hint!("test: a moment worth a reboot", 1.0).await;
                state.publish(&armed, false);
                if time.sleep(Duration::from_millis(10)).await.is_err() {
                    return;
                }
            }
        });
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Hints from outside any process for the whole chaos window: every hint
/// must resolve, since a workload has no process to reboot.
struct OutsideHinter {
    resolved: Arc<AtomicU64>,
}

#[async_trait]
impl Workload for OutsideHinter {
    fn name(&self) -> &'static str {
        "outside_hinter"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        for _ in 0..300 {
            hint!("test: a workload moment", 1.0).await;
            self.resolved.fetch_add(1, Ordering::Relaxed);
            ctx.time()
                .sleep(Duration::from_millis(10))
                .await
                .map_err(|e| SimulationError::InvalidState(format!("sleep: {e}")))?;
        }
        Ok(())
    }
}

/// Run `seeds` seeds of three hinting processes under one crash-only
/// attrition regime with `max_dead`; returns (kills at a hint, workload
/// hints resolved). With `vetoed`, the harness's veto refuses and counts
/// every reboot.
fn run(
    max_dead: usize,
    seeds: usize,
    check_determinism: bool,
    vetoed: Option<&Arc<AtomicU64>>,
) -> (u64, u64) {
    let vetoed = vetoed.cloned();
    let killed_at_hint = Arc::new(AtomicU64::new(0));
    let resolved = Arc::new(AtomicU64::new(0));
    let process_kills = Arc::clone(&killed_at_hint);
    let workload_resolved = Arc::clone(&resolved);
    let mut builder = SimulationBuilder::new()
        .processes(3, move || {
            Box::new(Hinter {
                killed_at_hint: Arc::clone(&process_kills),
                vetoed: vetoed.clone(),
            })
        })
        .workload_factory(move || {
            Box::new(OutsideHinter {
                resolved: Arc::clone(&workload_resolved),
            })
        })
        .enable_chaos([Chaos::Attrition {
            config: Attrition {
                max_dead,
                prob_graceful: 0.0,
                prob_crash: 1.0,
                prob_wipe: 0.0,
                recovery_delay_ms: Some(50..200),
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::Any,
            },
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(5))
        .set_iterations(seeds);
    if check_determinism {
        builder = builder.check_determinism();
    }
    let report = builder.run().expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report:?}");
    (
        killed_at_hint.load(Ordering::Relaxed),
        resolved.load(Ordering::Relaxed),
    )
}

#[test]
fn a_hint_reboots_its_process_under_the_attrition_regime() {
    // A point is activated on half of the runs, so a seed may well never
    // fire it: 20 seeds leave one chance in a million of no hinted reboot.
    let seeds = 20;
    let (killed_at_hint, resolved) = run(1, seeds, false, None);
    assert!(
        killed_at_hint > 0,
        "some seed rebooted a process at its hint"
    );
    assert_eq!(
        resolved,
        300 * seeds as u64,
        "every hint from a workload resolves"
    );
}

#[test]
fn a_regime_that_never_reboots_never_reboots_on_a_hint() {
    let (killed_at_hint, _) = run(0, 5, false, None);
    assert_eq!(killed_at_hint, 0, "max_dead = 0 refuses every hint");
}

#[test]
fn a_hinted_reboot_replays_deterministically() {
    // Every seed runs twice under the canary; a divergence fails the run.
    // 16 seeds: one chance in 65,536 that the point never activates.
    let (killed_at_hint, _) = run(1, 16, true, None);
    assert!(killed_at_hint > 0, "the canary saw hinted reboots");
}

#[test]
fn the_harness_veto_is_asked_last_and_a_refusal_leaves_the_process_alive() {
    let vetoed = Arc::new(AtomicU64::new(0));
    let (killed_at_hint, _) = run(1, 20, false, Some(&vetoed));
    assert_eq!(killed_at_hint, 0, "a refused reboot never lands");
    assert!(
        vetoed.load(Ordering::Relaxed) > 0,
        "the veto was asked once a reboot fit the regime"
    );
    let never_asked = Arc::new(AtomicU64::new(0));
    run(0, 5, false, Some(&never_asked));
    assert_eq!(
        never_asked.load(Ordering::Relaxed),
        0,
        "a reboot the regime refuses never reaches the veto"
    );
}
