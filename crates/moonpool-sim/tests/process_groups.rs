//! Process groups and role-aware attrition.
//!
//! Covers independent `.processes()` groups on one builder (each with its own
//! per-seed count draw and its own `10.0.{group}.x` IP range), the
//! `WorkloadTopology` / `FaultContext` group accessors, the
//! `Attrition::victims` filter (never rebooting outside the pool, replaying
//! identically for one seed), and one attrition regime per group.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, NetworkProvider, Process,
    SimContext, SimulationBuilder, SimulationError, SimulationResult, TcpListenerTrait,
    TimeProvider, Workload, assert_always,
};

/// A boot log shared with the process factories: every `run()` appends the
/// booting process's IP, so reboots show up as repeated entries.
type BootLog = Arc<Mutex<Vec<String>>>;

fn boot_log() -> BootLog {
    Arc::new(Mutex::new(Vec::new()))
}

fn booted_ips(log: &BootLog) -> Vec<String> {
    log.lock()
        .expect("Mutex poisoned: prior task panicked")
        .clone()
}

/// A process that checks its own group identity on every boot, records the
/// boot, binds a listener, and idles until shutdown.
struct RoleProcess {
    role: &'static str,
    boots: BootLog,
}

impl RoleProcess {
    fn factory(role: &'static str, boots: &BootLog) -> impl Fn() -> Box<dyn Process> + 'static {
        let boots = boots.clone();
        move || {
            Box::new(RoleProcess {
                role,
                boots: boots.clone(),
            })
        }
    }
}

#[async_trait]
impl Process for RoleProcess {
    fn name(&self) -> &str {
        self.role
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let topo = ctx.topology();
        let members = topo.ips_in_group(self.role);
        // A process is numbered within its own group: `client_id` indexes the
        // group's members and `client_count` is the group's size.
        assert_always!(
            topo.my_group() == Some(self.role),
            "process_groups: a process knows its own group",
            { "ip" => ctx.my_ip(), "group" => format!("{:?}", topo.my_group()) }
        );
        assert_always!(
            ctx.client_count() == members.len(),
            "process_groups: client_count is the group size",
            { "ip" => ctx.my_ip(), "count" => ctx.client_count(), "members" => members.len() }
        );
        assert_always!(
            members.get(ctx.client_id()).is_some_and(|ip| ip == ctx.my_ip()),
            "process_groups: client_id indexes the group's members",
            { "ip" => ctx.my_ip(), "id" => ctx.client_id() }
        );
        self.boots
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push(ctx.my_ip().to_string());

        let listener = ctx.network().bind(ctx.my_ip()).await?;
        loop {
            moonpool_sim::select! {
                biased;
                _ = listener.accept() => {}
                () = ctx.shutdown().cancelled() => return Ok(()),
            }
        }
    }
}

/// Run for a fixed sim-time duration, then return so the iteration completes.
struct TimedWorkload(Duration);

#[async_trait]
impl Workload for TimedWorkload {
    fn name(&self) -> &'static str {
        "timed"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        ctx.time()
            .sleep(self.0)
            .await
            .map_err(|e| SimulationError::InvalidState(format!("sleep failed: {e}")))
    }
}

/// Every group gets its own attrition regime; the base is shared.
fn regime(max_dead: usize, victims: AttritionVictims) -> Attrition {
    Attrition {
        max_dead,
        prob_graceful: 0.3,
        prob_crash: 0.5,
        prob_wipe: 0.2,
        recovery_delay_ms: None,
        grace_period_ms: None,
        scope: AttritionScope::PerProcess,
        victims,
    }
}

// ============================================================================
// Test: two groups draw independent counts and land on contiguous IP ranges.
// ============================================================================

/// The `(acceptors, matchmakers)` shape every seed drew.
type Shapes = Arc<Mutex<Vec<(usize, usize)>>>;

/// Validates the group layout a seed drew and records its shape.
struct ShapeWorkload {
    shapes: Shapes,
}

#[async_trait]
impl Workload for ShapeWorkload {
    fn name(&self) -> &'static str {
        "shape"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let topo = ctx.topology();
        let acceptors = topo.ips_in_group("acceptor");
        let matchmakers = topo.ips_in_group("matchmaker");
        let expected_acceptors: Vec<String> = (1..=acceptors.len())
            .map(|n| format!("10.0.1.{n}"))
            .collect();
        let expected_matchmakers: Vec<String> = (1..=matchmakers.len())
            .map(|n| format!("10.0.2.{n}"))
            .collect();
        let ok = (3..=5).contains(&acceptors.len())
            && (0..=3).contains(&matchmakers.len())
            && acceptors == expected_acceptors
            && matchmakers == expected_matchmakers
            && topo.all_process_ips().len() == acceptors.len() + matchmakers.len()
            && topo.groups() == ["acceptor", "matchmaker"]
            && topo.group_for("10.0.1.1") == Some("acceptor")
            && topo.my_group().is_none();
        if !ok {
            return Err(SimulationError::InvalidState(format!(
                "bad group layout: acceptors={acceptors:?} matchmakers={matchmakers:?} \
                 all={:?} groups={:?}",
                topo.all_process_ips(),
                topo.groups()
            )));
        }
        self.shapes
            .lock()
            .expect("Mutex poisoned: prior task panicked")
            .push((acceptors.len(), matchmakers.len()));
        Ok(())
    }
}

#[test]
fn two_groups_draw_independent_counts_on_contiguous_ranges() {
    let shapes: Shapes = Arc::new(Mutex::new(Vec::new()));
    let boots = boot_log();
    let seeds: Vec<u64> = (1..=16).collect();
    let report = SimulationBuilder::new()
        .processes(3..=5, RoleProcess::factory("acceptor", &boots))
        .processes(0..=3, RoleProcess::factory("matchmaker", &boots))
        .workload(ShapeWorkload {
            shapes: shapes.clone(),
        })
        .set_iterations(seeds.len())
        .set_debug_seeds(seeds)
        .run();

    assert_eq!(report.failed_runs, 0, "group layout should hold: {report}");
    assert!(report.assertion_violations.is_empty(), "{report}");
    let shapes = shapes.lock().expect("Mutex poisoned: prior task panicked");
    assert_eq!(shapes.len(), 16);
    let acceptor_counts: std::collections::BTreeSet<usize> =
        shapes.iter().map(|&(a, _)| a).collect();
    let matchmaker_counts: std::collections::BTreeSet<usize> =
        shapes.iter().map(|&(_, m)| m).collect();
    assert!(
        acceptor_counts.len() > 1,
        "acceptor count never varied across seeds: {shapes:?}"
    );
    assert!(
        matchmaker_counts.len() > 1,
        "matchmaker count never varied across seeds: {shapes:?}"
    );
    // Both groups boot once per seed, nothing more, with no attrition.
    let booted = booted_ips(&boots);
    let total: usize = shapes.iter().map(|&(a, m)| a + m).sum();
    assert_eq!(booted.len(), total);
}

#[test]
#[should_panic(expected = "already registered")]
fn registering_the_same_group_twice_panics() {
    let boots = boot_log();
    let _ = SimulationBuilder::new()
        .processes(3, RoleProcess::factory("acceptor", &boots))
        .processes(2, RoleProcess::factory("acceptor", &boots));
}

// ============================================================================
// Test: a victim filter never reboots a process outside its pool.
// ============================================================================

/// Run five seeds of filtered attrition over 3 acceptors + 2 spares and
/// return the boot log.
fn filtered_attrition(victims: AttritionVictims, seeds: Vec<u64>) -> Vec<String> {
    let boots = boot_log();
    let report = SimulationBuilder::new()
        .processes(3, RoleProcess::factory("acceptor", &boots))
        .tags(&[("role", &["voter"])])
        .expect("tags follow a group")
        .processes(2, RoleProcess::factory("spare", &boots))
        .tags(&[("role", &["standby"])])
        .expect("tags follow a group")
        .workload(TimedWorkload(Duration::from_secs(25)))
        .enable_chaos([Chaos::Attrition {
            config: regime(1, victims),
            mode: ChaosMode::Random,
        }])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(seeds.len())
        .set_debug_seeds(seeds)
        .run();
    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(report.assertion_violations.is_empty(), "{report}");
    booted_ips(&boots)
}

/// Every reboot lands in the pool: spares boot exactly once per seed, and
/// acceptors boot more often than that because attrition did fire.
fn assert_only_pool_rebooted(booted: &[String], seeds: usize) {
    let spare_boots = booted.iter().filter(|ip| ip.starts_with("10.0.2.")).count();
    let acceptor_boots = booted.iter().filter(|ip| ip.starts_with("10.0.1.")).count();
    assert_eq!(
        spare_boots,
        2 * seeds,
        "a spare was rebooted despite the victim filter: {booted:?}"
    );
    assert!(
        acceptor_boots > 3 * seeds,
        "attrition never rebooted an acceptor: {booted:?}"
    );
}

#[test]
fn group_victim_filter_never_reboots_outside_the_group() {
    let seeds: Vec<u64> = vec![1, 2, 3, 4, 5];
    let booted = filtered_attrition(AttritionVictims::group("acceptor"), seeds.clone());
    assert_only_pool_rebooted(&booted, seeds.len());
}

#[test]
fn tag_victim_filter_never_reboots_outside_the_tag() {
    let seeds: Vec<u64> = vec![1, 2, 3, 4, 5];
    let booted = filtered_attrition(AttritionVictims::tagged("role", "voter"), seeds.clone());
    assert_only_pool_rebooted(&booted, seeds.len());
}

#[test]
fn filtered_attrition_replays_identically_for_one_seed() {
    let first = filtered_attrition(AttritionVictims::group("acceptor"), vec![42]);
    let second = filtered_attrition(AttritionVictims::group("acceptor"), vec![42]);
    assert!(
        first.len() > 3,
        "seed 42 should reboot at least one acceptor: {first:?}"
    );
    assert_eq!(first, second, "same seed, same boot sequence");
}

// ============================================================================
// Test: one attrition regime per group, each spending its own budget.
// ============================================================================

#[test]
fn one_attrition_regime_per_group_reboots_both_groups() {
    let boots = boot_log();
    let seeds: Vec<u64> = vec![1, 2, 3, 4, 5];
    let report = SimulationBuilder::new()
        .processes(3, RoleProcess::factory("acceptor", &boots))
        .processes(3, RoleProcess::factory("matchmaker", &boots))
        .workload(TimedWorkload(Duration::from_secs(25)))
        .enable_chaos([
            Chaos::Attrition {
                config: regime(1, AttritionVictims::group("acceptor")),
                mode: ChaosMode::Random,
            },
            Chaos::Attrition {
                config: regime(2, AttritionVictims::group("matchmaker")),
                mode: ChaosMode::Random,
            },
        ])
        .chaos_duration(Duration::from_secs(10))
        .set_iterations(seeds.len())
        .set_debug_seeds(seeds.clone())
        .run();

    assert_eq!(report.failed_runs, 0, "{report}");
    assert!(report.assertion_violations.is_empty(), "{report}");
    let booted = booted_ips(&boots);
    let acceptor_boots = booted.iter().filter(|ip| ip.starts_with("10.0.1.")).count();
    let matchmaker_boots = booted.iter().filter(|ip| ip.starts_with("10.0.2.")).count();
    assert!(
        acceptor_boots > 3 * seeds.len(),
        "the acceptor regime never fired: {booted:?}"
    );
    assert!(
        matchmaker_boots > 3 * seeds.len(),
        "the matchmaker regime never fired: {booted:?}"
    );
}
