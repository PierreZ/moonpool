//! The seed plan (`SimulationBuilder::seed_plan`): one hook draws a group's
//! count, its attrition budget, whether its outages strike, and values the
//! run reads from its state.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, LocalityConfig, NetworkProvider,
    Outage, Process, SeedPlan, SimContext, SimulationBuilder, SimulationError, SimulationResult,
    TimeProvider, Workload,
};

/// Every boot's IP, so a reboot shows up as a repeated entry.
type BootLog = Arc<Mutex<Vec<String>>>;

fn boots(log: &BootLog) -> Vec<String> {
    log.lock()
        .expect("Mutex poisoned: prior task panicked")
        .clone()
}

/// Records its boot, binds a listener, and idles until shutdown.
struct Idle {
    boots: BootLog,
}

#[async_trait]
impl Process for Idle {
    fn name(&self) -> &'static str {
        "machine"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.boots
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(ctx.my_ip().to_string());
        let _listener = ctx.network().bind(ctx.my_ip()).await?;
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

fn idle(boots: &BootLog) -> impl Fn() -> Box<dyn Process> + 'static {
    let boots = boots.clone();
    move || {
        Box::new(Idle {
            boots: boots.clone(),
        })
    }
}

/// What each seed saw: the machines' count and the plan's published value.
type Seen = Arc<Mutex<Vec<(usize, Option<u64>)>>>;

/// Records the machine count and the plan's `large` value, then runs for a
/// fixed time.
struct Observer {
    seen: Seen,
    run: Duration,
}

#[async_trait]
impl Workload for Observer {
    fn name(&self) -> &'static str {
        "observer"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let machines = ctx.topology().ips_in_group("machine").len();
        let large = ctx.state().get::<u64>("large");
        self.seen
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push((machines, large));
        ctx.time()
            .sleep(self.run)
            .await
            .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))
    }
}

/// A larger group on half the seeds: 7 machines and the value `1`.
fn larger_on_half(plan: &mut SeedPlan) {
    if moonpool_sim::sim_random_bool(0.5) {
        plan.set_count("machine", 7);
        plan.publish("large", 1);
    }
}

#[test]
fn a_plan_sets_a_count_on_a_share_of_the_seeds_and_publishes_it() {
    let log: BootLog = Arc::default();
    let seen: Seen = Arc::default();
    let seeds: Vec<u64> = (1..=24).collect();
    let report = SimulationBuilder::new()
        .processes(1..=3, idle(&log))
        .workload(Observer {
            seen: seen.clone(),
            run: Duration::from_millis(10),
        })
        .seed_plan(larger_on_half)
        .set_iterations(seeds.len())
        .set_debug_seeds(seeds)
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    let seen = seen.lock().expect("Mutex poisoned: prior task panicked");
    assert_eq!(seen.len(), 24);
    for &(machines, large) in seen.iter() {
        match large {
            Some(1) => assert_eq!(machines, 7, "{seen:?}"),
            None => assert!((1..=3).contains(&machines), "{seen:?}"),
            Some(other) => panic!("unexpected published value {other}"),
        }
    }
    assert!(seen.iter().any(|&(_, large)| large.is_some()), "{seen:?}");
    assert!(seen.iter().any(|&(_, large)| large.is_none()), "{seen:?}");
}

#[test]
fn a_plan_replays_the_same_for_one_seed() {
    let run = || {
        let log: BootLog = Arc::default();
        let seen: Seen = Arc::default();
        SimulationBuilder::new()
            .processes(1..=3, idle(&log))
            .workload(Observer {
                seen: seen.clone(),
                run: Duration::from_millis(10),
            })
            .seed_plan(larger_on_half)
            .set_iterations(8)
            .set_debug_seeds((40..48).collect())
            .run()
            .expect("simulation configuration is valid");
        seen.lock()
            .expect("Mutex poisoned: prior task panicked")
            .clone()
    };
    assert_eq!(run(), run());
}

/// A regime that reboots a machine as often as its budget allows.
fn attrition() -> Attrition {
    Attrition {
        max_dead: 1,
        prob_graceful: 0.0,
        prob_crash: 1.0,
        prob_wipe: 0.0,
        recovery_delay_ms: Some(100..200),
        grace_period_ms: None,
        scope: AttritionScope::PerProcess,
        victims: AttritionVictims::group("machine"),
        wipe_budget: None,
    }
}

/// The boots of four seeds of three machines under `chaos`, with `plan`.
fn boots_under(chaos: Chaos, plan: impl Fn(&mut SeedPlan) + 'static) -> Vec<String> {
    let log: BootLog = Arc::default();
    let report = SimulationBuilder::new()
        .processes(3, idle(&log))
        .workload(Observer {
            seen: Arc::default(),
            run: Duration::from_secs(20),
        })
        .enable_chaos([chaos])
        .seed_plan(plan)
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(4)
        .set_debug_seeds((1..=4).collect())
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(report.assertion_violations.is_empty(), "{report}");
    boots(&log)
}

#[test]
fn a_plan_budget_replaces_the_victims_max_dead() {
    let regime = || Chaos::Attrition {
        config: attrition(),
        mode: ChaosMode::Random,
    };
    // The base budget reboots machines: more boots than processes.
    assert!(boots_under(regime(), |_| {}).len() > 12);
    // A budget of zero for the machines reboots none.
    assert_eq!(
        boots_under(regime(), |plan| plan.set_max_dead("machine", 0)).len(),
        12
    );
    // A budget for another group leaves the machines' regime alone.
    assert!(boots_under(regime(), |plan| plan.set_max_dead("other", 0)).len() > 12);
}

#[test]
fn a_calmed_group_has_no_outage() {
    let outage = || Chaos::Outage {
        config: Outage {
            start: Duration::from_secs(1)..Duration::from_secs(2),
            down: Duration::from_secs(1)..Duration::from_secs(2),
            straggler: None,
            ..Outage::groups(["machine"])
        },
        mode: ChaosMode::Random,
    };
    // The outage reboots every machine of every seed.
    assert_eq!(boots_under(outage(), |_| {}).len(), 24);
    // Calmed, it strikes nothing.
    assert_eq!(boots_under(outage(), |plan| plan.calm("machine")).len(), 12);
}

#[test]
#[should_panic(expected = "not a .processes() group")]
fn a_plan_cannot_set_the_count_of_a_cluster_group() {
    let log: BootLog = Arc::default();
    let _ = SimulationBuilder::new()
        .cluster(LocalityConfig::new(3..=3, 1, 1, 1), idle(&log))
        .workload(Observer {
            seen: Arc::default(),
            run: Duration::from_millis(10),
        })
        .seed_plan(|plan| plan.set_count("machine", 5))
        .set_iterations(1)
        .set_debug_seeds(vec![1])
        .run();
}
