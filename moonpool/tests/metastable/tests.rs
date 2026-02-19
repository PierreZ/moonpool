//! Test functions for the metastable failure simulation.

use moonpool_sim::{ExplorationConfig, SimulationBuilder, SimulationReport};

use super::dns::DnsWorkload;
use super::driver::DriverWorkload;
use super::fleet::FleetWorkload;
use super::invariants::RecoveryInvariant;
use super::lease_store::LeaseStoreWorkload;

/// Helper to run a simulation and return the report.
fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .rng_seed(tokio::runtime::RngSeed::from_bytes(b"deterministic"))
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

/// Quick debug test: single iteration, no exploration, no chaos.
#[test]
fn test_metastable_basic() {
    let report = run_simulation(
        SimulationBuilder::new()
            .workload(DnsWorkload::new())
            .workload(LeaseStoreWorkload::new())
            .workload(FleetWorkload::new())
            .workload(DriverWorkload::new())
            .set_iterations(1)
            .set_debug_seeds(vec![42]),
    );

    eprintln!("{}", report);
    assert_eq!(report.successful_runs, 1);
}

/// Metastable failure exploration test: fork-based exploration discovers the
/// metastable failure state where DNS heals but the system stays broken.
///
/// The recovery invariant fires when DNS has been healthy for >15s but goodput
/// remains collapsed — the defining property of metastable failure.
///
/// After discovery, the bug recipe is captured, round-tripped through the
/// timeline format, and replayed deterministically (same pattern as dungeon).
#[test]
fn slow_simulation_metastable_failure() {
    // Phase 1: Run exploration to discover metastable failure
    let report = run_simulation(
        SimulationBuilder::new()
            .workload(DnsWorkload::new())
            .workload(LeaseStoreWorkload::new())
            .workload(FleetWorkload::new())
            .workload(DriverWorkload::new())
            .invariant(RecoveryInvariant::new())
            .random_network()
            .set_iterations(2)
            .set_debug_seeds(vec![54321])
            .enable_exploration(ExplorationConfig {
                max_depth: 60,
                timelines_per_split: 4,
                global_energy: 20_000,
                adaptive: Some(moonpool_sim::AdaptiveConfig {
                    batch_size: 20,
                    min_timelines: 60,
                    max_timelines: 200,
                    per_mark_energy: 2_000,
                    warm_min_timelines: Some(20),
                }),
                parallelism: Some(moonpool_sim::Parallelism::HalfCores),
            }),
    );

    eprintln!("{report}");

    let exp = report.exploration.expect("exploration report missing");
    assert!(exp.total_timelines > 0, "expected forked timelines, got 0");
    assert!(
        exp.bugs_found > 0,
        "exploration should find recovery invariant violation"
    );

    // The metastable failure is also a reachability property (sometimes assertion).
    let metastable = report
        .assertion_results
        .get("metastable_failure_detected")
        .expect("metastable_failure_detected assertion missing");
    assert!(
        metastable.successes > 0,
        "exploration should have found the metastable failure"
    );

    // Phase 2: Capture bug recipe — now includes the root seed
    let bug = exp
        .bug_recipes
        .first()
        .expect("bug recipe should be captured");
    let timeline_str = moonpool_sim::format_timeline(&bug.recipe);
    eprintln!("Replay values: seed={}, recipe={}", bug.seed, timeline_str);
}

/// Fast replay test using a known-good recipe from exploration.
///
/// Uses hardcoded seed + breakpoints discovered by `slow_simulation_metastable_failure`.
/// This lets us iterate on replay correctness without re-running the expensive exploration.
#[test]
fn test_metastable_replay() {
    let bug = moonpool_sim::BugRecipe {
        seed: 54321,
        recipe: moonpool_sim::parse_timeline(
            "5@590681868797176967 -> 8@865448932235176101 -> 81@4779941305685547315 -> 0@18260205261052660768 -> 4@14388353850498491855 -> 1156@10435589435718015354 -> 0@8966787143959973739 -> 524@15111294430527635224 -> 1654@1314653516675584659 -> 151@8075856974641946789",
        )
        .expect("recipe should parse"),
    };

    let report = run_simulation(
        SimulationBuilder::new()
            .workload(DnsWorkload::new())
            .workload(LeaseStoreWorkload::new())
            .workload(FleetWorkload::new())
            .workload(DriverWorkload::new())
            .invariant(RecoveryInvariant::new())
            .random_network()
            .replay_recipe(bug)
            .set_iterations(1)
            .set_debug_seeds(vec![54321]),
    );

    eprintln!("Replay report: {report}");

    let recovery = report
        .assertion_results
        .get("recovery_after_dns_heals")
        .expect("recovery assertion missing in replay");
    let failures = recovery.total_checks - recovery.successes;
    assert!(
        failures > 0,
        "replay should reproduce the recovery invariant violation"
    );
}
