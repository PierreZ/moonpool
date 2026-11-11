//! End-to-end integration test for swarm testing (`SimulationBuilder::swarm`).
//!
//! Verifies that the swarm-subset wiring (the per-seed subset draws plus the
//! swarm branch in `build_sim_for_iteration`) drives a full run cleanly across
//! many seeds: every seed builds its own per-seed fault subset and the
//! orchestrator completes. The per-seed subset logic itself (which families are
//! enabled, determinism, the all-off case) is unit-tested in
//! `crates/moonpool-sim/src/network/config.rs`.
//!
//! The workload deliberately does not open connections. Under some swarm subsets
//! `ConnectFailureMode::Probabilistic` makes `connect()` hang forever — correct
//! chaos behavior, but it would make this plumbing test depend on which subsets
//! the entropy-seeded iteration happens to draw. `bind()` has no failure mode,
//! so it exercises the swarm-configured network provider without that coupling.

use async_trait::async_trait;
use moonpool_sim::{Chaos, ChaosMode, NetworkProvider, SimContext, SimulationBuilder, Workload};

/// Minimal workload: touches the swarm-configured network provider via `bind`
/// and fires a coverage gate. Always completes, regardless of fault subset.
struct SwarmSmokeWorkload;

#[async_trait]
impl Workload for SwarmSmokeWorkload {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn run(&mut self, ctx: &SimContext) -> moonpool_sim::SimulationResult<()> {
        moonpool_sim::assert_sometimes!(true, "swarm run reached workload");
        // bind has no failure/hang mode, so this can't deadlock under any subset.
        if let Ok(listener) = ctx.network().bind("server").await {
            drop(listener);
        }
        Ok(())
    }
}

/// A swarm run over many seeds must build a per-seed subset and complete every
/// seed without deadlocks, panics, or orchestration failures.
#[test]
fn swarm_runs_clean_across_seeds() {
    let report = SimulationBuilder::new()
        .enable_chaos([Chaos::Network(ChaosMode::Swarm)])
        .set_iterations(50)
        .workload(SwarmSmokeWorkload)
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(
        report.failed_runs, 0,
        "swarm run had {} failed seeds",
        report.failed_runs
    );
    assert_eq!(report.iterations, 50, "expected all 50 seeds to run");
}

/// `Chaos::BuggifyKnobs` layered on top of swarm must build a per-seed config
/// (with occasional knob-value spikes applied in `build_sim_for_iteration`) and
/// complete every seed cleanly. Exercises both the network and storage
/// `apply_buggify_knobs` wiring. The buggify network spikes (clog/partition/
/// random-close magnitudes) never touch `connect_failure_mode`, so the bind-only
/// workload still can't deadlock.
#[test]
fn buggify_knobs_runs_clean_across_seeds() {
    let report = SimulationBuilder::new()
        .enable_chaos([
            Chaos::Network(ChaosMode::Swarm),
            Chaos::Storage(ChaosMode::Swarm),
            Chaos::BuggifyKnobs,
        ])
        .set_iterations(50)
        .workload(SwarmSmokeWorkload)
        .run()
        .expect("simulation configuration is valid");

    assert_eq!(
        report.failed_runs, 0,
        "buggify-knobs run had {} failed seeds",
        report.failed_runs
    );
    assert_eq!(report.iterations, 50, "expected all 50 seeds to run");
}

/// Collects the `chaos_families` fault events of each seed.
struct Families {
    cursor: std::cell::Cell<usize>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl moonpool_sim::Invariant for Families {
    fn name(&self) -> &'static str {
        "families"
    }

    fn observe(&self, q: &dyn moonpool_sim::TraceQuery, _sim_time_ms: u64) {
        for event in q.since(moonpool_sim::SIM_FAULT_EVENT_NAME, &self.cursor) {
            if event.str("kind") == Some("chaos_families") {
                self.seen.lock().expect("families lock").push((
                    event.str("network").unwrap_or("?").to_string(),
                    event.str("storage").unwrap_or("?").to_string(),
                ));
            }
        }
    }

    fn reset(&mut self) {
        self.cursor.set(0);
    }
}

/// Each swarm seed records the families it drew, once, in one
/// `chaos_families` event: a subset of the families, never the same subset
/// for every seed, and a family that the mask removed is never listed.
#[test]
fn each_swarm_seed_records_its_families_once() {
    use moonpool_sim::{NetworkFault, NetworkFaultMask, StorageFault};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let report = SimulationBuilder::new()
        .enable_chaos([
            Chaos::Network(ChaosMode::Swarm),
            Chaos::Storage(ChaosMode::Swarm),
        ])
        .network_fault_mask(NetworkFaultMask::all().without(NetworkFault::BitFlip))
        .set_iterations(20)
        .workload(SwarmSmokeWorkload)
        .invariant(Families {
            cursor: std::cell::Cell::new(0),
            seen: seen.clone(),
        })
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0);
    let seen = seen.lock().expect("families lock");
    assert_eq!(seen.len(), 20, "one chaos_families event per seed");
    let network: Vec<&str> = NetworkFault::ALL.iter().map(|f| f.name()).collect();
    let storage: Vec<&str> = StorageFault::ALL.iter().map(|f| f.name()).collect();
    for (net, sto) in seen.iter() {
        for name in net.split(',').filter(|n| !n.is_empty()) {
            assert!(network.contains(&name), "{name} is a network family");
            assert_ne!(name, "bit_flip", "the mask removed bit_flip");
        }
        for name in sto.split(',').filter(|n| !n.is_empty()) {
            assert!(storage.contains(&name), "{name} is a storage family");
        }
    }
    assert!(
        seen.iter().any(|s| s != &seen[0]),
        "the seeds draw different subsets"
    );
}

/// A run without network or storage chaos records no `chaos_families` event.
#[test]
fn a_run_without_chaos_records_no_families() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let report = SimulationBuilder::new()
        .set_iterations(3)
        .workload(SwarmSmokeWorkload)
        .invariant(Families {
            cursor: std::cell::Cell::new(0),
            seen: seen.clone(),
        })
        .run()
        .expect("simulation configuration is valid");
    assert_eq!(report.failed_runs, 0);
    assert!(seen.lock().expect("families lock").is_empty());
}
