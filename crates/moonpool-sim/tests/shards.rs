//! Shards: the code declares its replication with `shard!`, and the gate
//! keeps every destructive fault survivable (see `moonpool_sim::Shards`).

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::shard::{Replica, Replication};
use moonpool_sim::{
    Aim, Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, Core, Invariant, Process,
    SIM_FAULT_EVENT_NAME, ShardCounts, ShardFaults, SimContext, SimulationBuilder,
    SimulationReport, SimulationResult, TaskProvider, TimeProvider, TraceQuery, Workload,
    assert_always, hint, shard,
};

/// The event a [`Node`] emits when its copy healed after a wipe.
const HEALED: &str = "shard_healed";

/// The replicas of the `node` group, as `shard!` names them.
fn replicas(ctx: &SimContext) -> Vec<Replica> {
    ctx.topology()
        .ips_in_group("node")
        .into_iter()
        .map(Replica::new)
        .collect()
}

/// A replica of one shard that tolerates one down and one lost copy. Each
/// boot declares the shard, hints a moment every 50 ms, and says its copy
/// caught up one second after booting.
struct Node;

#[async_trait]
impl Process for Node {
    fn name(&self) -> &'static str {
        "node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        shard!(
            "orders",
            current = Replication::tolerates(replicas(ctx), 1, 1)
        );
        let time = ctx.time().clone();
        let _worker = ctx.task().spawn_task("hinting worker", async move {
            loop {
                hint!("shards test: a moment", 0.05).await;
                if time.sleep(Duration::from_millis(50)).await.is_err() {
                    return;
                }
            }
        });
        if ctx.time().sleep(Duration::from_secs(1)).await.is_err() {
            return Ok(());
        }
        if ctx.shards().caught_up("orders", &Replica::new(ctx.my_ip())) {
            tracing::info!(target: "shards_test", "shard_healed");
        }
        ctx.shutdown().cancelled().await;
        Ok(())
    }
}

/// Keeps the run alive, then hands the book's counts to the test.
struct Timed {
    run_for: Duration,
    counts: Arc<Mutex<Vec<ShardCounts>>>,
}

#[async_trait]
impl Workload for Timed {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let _ = ctx.time().sleep(self.run_for).await;
        Ok(())
    }

    async fn check(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        self.counts
            .lock()
            .expect("counts lock")
            .push(ctx.shards().counts());
        Ok(())
    }
}

/// What the invariant saw across one run.
#[derive(Debug, Default, Clone)]
struct Observed {
    kills: usize,
    wipes: usize,
    most_down: usize,
    most_unhealed: usize,
}

/// Tracks the processes down and the wiped copies not yet healed from the
/// fault timeline, and asserts the shard's tolerance at every kill.
struct Tolerance {
    fault_cursor: Cell<usize>,
    heal_cursor: Cell<usize>,
    down: RefCell<BTreeSet<String>>,
    unhealed: RefCell<BTreeSet<String>>,
    observed: Arc<Mutex<Observed>>,
}

impl Invariant for Tolerance {
    fn name(&self) -> &'static str {
        "shard_tolerance"
    }

    fn observe(&self, q: &dyn TraceQuery, _sim_time_ms: u64) {
        let mut fresh = q.since(SIM_FAULT_EVENT_NAME, &self.fault_cursor);
        fresh.extend(q.since(HEALED, &self.heal_cursor));
        fresh.sort_by_key(|event| event.seq);
        let mut observed = self.observed.lock().expect("observations lock");
        let mut down = self.down.borrow_mut();
        let mut unhealed = self.unhealed.borrow_mut();
        for event in fresh {
            if event.name == HEALED {
                unhealed.remove(&event.source);
                continue;
            }
            let Some(ip) = event.str("ip") else {
                continue;
            };
            match (event.str("kind"), event.str("cause").unwrap_or("")) {
                (Some("process_force_kill"), "restart_in_place") => {}
                (Some("process_force_kill"), cause) => {
                    down.insert(ip.to_string());
                    observed.kills += 1;
                    if cause == "crash_and_wipe" {
                        unhealed.insert(ip.to_string());
                        observed.wipes += 1;
                    }
                    observed.most_down = observed.most_down.max(down.len());
                    observed.most_unhealed = observed.most_unhealed.max(unhealed.len());
                    assert_always!(down.len() <= 1, "shards: one replica down at most", {
                        "down" => format!("{down:?}")
                    });
                    assert_always!(
                        unhealed.len() <= 1,
                        "shards: one copy lost at most",
                        { "unhealed" => format!("{unhealed:?}") }
                    );
                }
                (Some("process_restart"), _) => {
                    down.remove(ip);
                }
                _ => {}
            }
        }
    }

    fn reset(&mut self) {
        self.fault_cursor.set(0);
        self.heal_cursor.set(0);
        self.down.borrow_mut().clear();
        self.unhealed.borrow_mut().clear();
    }
}

fn run_gate(seed: u64) -> (SimulationReport, Observed, ShardCounts) {
    let observed = Arc::new(Mutex::new(Observed::default()));
    let counts = Arc::new(Mutex::new(Vec::new()));
    let report = SimulationBuilder::new()
        .processes(3, || Box::new(Node))
        .workload(Timed {
            run_for: Duration::from_secs(40),
            counts: Arc::clone(&counts),
        })
        .enable_chaos([Chaos::Attrition {
            // max_dead 3 never bounds the faults: the gate does.
            config: Attrition {
                max_dead: 3,
                prob_graceful: 0.0,
                prob_crash: 0.5,
                prob_wipe: 0.5,
                recovery_delay_ms: Some(500..3000),
                grace_period_ms: None,
                scope: AttritionScope::PerProcess,
                victims: AttritionVictims::group("node"),
                wipe_budget: None,
            },
            mode: ChaosMode::Random,
        }])
        .invariant(Tolerance {
            fault_cursor: Cell::new(0),
            heal_cursor: Cell::new(0),
            down: RefCell::default(),
            unhealed: RefCell::default(),
            observed: Arc::clone(&observed),
        })
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(1)
        .set_debug_seeds(vec![seed])
        .run()
        .expect("simulation configuration is valid");
    let observed = observed.lock().expect("observations lock").clone();
    let counts = counts
        .lock()
        .expect("counts lock")
        .pop()
        .unwrap_or_default();
    (report, observed, counts)
}

const SEEDS: [u64; 4] = [1, 7, 42, 999];

/// Under heavy attrition with `max_dead: 3`, a shard that tolerates one
/// down and one lost copy never has two replicas down or two copies lost
/// at once, and the counts show the downgrades.
#[test]
fn the_gate_keeps_a_three_replica_shard_within_its_tolerance() {
    let mut downgrades = 0;
    let mut wipes = 0;
    let mut healed = 0;
    for seed in SEEDS {
        let (report, observed, counts) = run_gate(seed);
        assert_eq!(
            report.failed_runs, 0,
            "seed {seed}: {:?}",
            report.assertion_violations
        );
        assert!(observed.most_down <= 1, "seed {seed}: {observed:?}");
        assert!(observed.most_unhealed <= 1, "seed {seed}: {observed:?}");
        assert!(observed.kills > 0, "seed {seed}: {observed:?}");
        assert!(counts.declarations >= 3, "seed {seed}: {counts:?}");
        downgrades += counts.downgrades_total();
        wipes += observed.wipes;
        healed += counts.healed;
    }
    assert!(downgrades > 0, "the gate downgraded some fault");
    assert!(wipes > 0, "some wipe landed");
    assert!(healed > 0, "a wiped copy healed");
}

/// The replicas of a five-replica shard that moves between nodes 1..=5 and
/// 2..=6. The node of rank 0 drives the moves; every node beats a heart in
/// the run's state.
struct Mover;

impl Mover {
    fn replication(nodes: &[String]) -> Replication {
        Replication::tolerates(nodes.iter().map(Replica::new).collect(), 2, 2)
    }
}

#[async_trait]
impl Process for Mover {
    fn name(&self) -> &'static str {
        "node"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let nodes = ctx.topology().ips_in_group("node");
        let r1 = Self::replication(&nodes[..5]);
        let r2 = Self::replication(&nodes[1..]);
        let me = ctx.my_ip().to_string();
        let leader = nodes.first() == Some(&me);
        if !leader {
            shard!("log", current = r1.clone());
        }
        let mut beat: u64 = 0;
        let mut on_r1 = true;
        loop {
            let (from, to) = if on_r1 { (&r1, &r2) } else { (&r2, &r1) };
            if leader {
                shard!("log", current = from.clone());
            }
            for _ in 0..10 {
                beat += 1;
                ctx.state().publish(&format!("beat:{me}"), beat);
                if ctx.time().sleep(Duration::from_millis(100)).await.is_err() {
                    return Ok(());
                }
            }
            if leader {
                shard!("log", current = from.clone(), next = to.clone());
            }
            for _ in 0..10 {
                beat += 1;
                ctx.state().publish(&format!("beat:{me}"), beat);
                if ctx.time().sleep(Duration::from_millis(100)).await.is_err() {
                    return Ok(());
                }
            }
            on_r1 = !on_r1;
        }
    }
}

/// Checks that the core's replicas beat after the chaos window.
struct CoreCheck {
    counts: Arc<Mutex<Vec<(ShardCounts, bool)>>>,
}

#[async_trait]
impl Workload for CoreCheck {
    fn name(&self) -> &'static str {
        "core_check"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        // The chaos window closes at 20 s; the core is set by then.
        let _ = ctx.time().sleep(Duration::from_secs(25)).await;
        let shards = ctx.shards();
        let core = shards.core("log").unwrap_or_default();
        let beat = |replica: &Replica| {
            ctx.state()
                .get::<u64>(&format!("beat:{}", replica.address()))
                .unwrap_or(0)
        };
        let before: Vec<u64> = core.iter().map(beat).collect();
        let _ = ctx.time().sleep(Duration::from_secs(2)).await;
        let after: Vec<u64> = core.iter().map(beat).collect();
        let alive = !core.is_empty() && before.iter().zip(&after).all(|(b, a)| a > b);
        self.counts
            .lock()
            .expect("counts lock")
            .push((shards.counts(), alive));
        Ok(())
    }
}

/// A moving five-replica shard under aimed, edge-pushing attrition
/// converges on its liveness core: every core replica runs after the chaos
/// window, and the counts show the aim, the edge and the core.
#[test]
fn a_moving_shard_converges_on_its_core() {
    let (mut aimed, mut edge, mut moves) = (0, 0, 0);
    for seed in SEEDS {
        let counts = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .processes(6, || Box::new(Mover))
            .workload(CoreCheck {
                counts: Arc::clone(&counts),
            })
            .enable_chaos([Chaos::Attrition {
                config: Attrition {
                    max_dead: 2,
                    prob_graceful: 0.0,
                    prob_crash: 1.0,
                    prob_wipe: 0.0,
                    recovery_delay_ms: Some(1000..4000),
                    grace_period_ms: None,
                    scope: AttritionScope::PerProcess,
                    victims: AttritionVictims::group("node"),
                    wipe_budget: None,
                },
                mode: ChaosMode::Random,
            }])
            .shard_faults(
                ShardFaults::default()
                    .aim(Aim::moves(8.0))
                    .edge(0.3)
                    .core(Core::RandomMinimal),
            )
            .chaos_duration(Duration::from_secs(20))
            .set_iterations(1)
            .set_debug_seeds(vec![seed])
            .run()
            .expect("simulation configuration is valid");
        assert_eq!(
            report.failed_runs, 0,
            "seed {seed}: {:?}",
            report.assertion_violations
        );
        let (counts, alive) = counts
            .lock()
            .expect("counts lock")
            .pop()
            .expect("check ran");
        assert!(alive, "seed {seed}: the core runs after the chaos window");
        assert_eq!(counts.core, 1, "seed {seed}: {counts:?}");
        aimed += counts.aimed;
        edge += counts.edge;
        moves += counts.moves_started;
    }
    assert!(moves > 0, "the shard moved");
    assert!(aimed > 0, "the aim struck a moving shard");
    assert!(edge > 0, "the edge pushed a shard to its minimum");
}
