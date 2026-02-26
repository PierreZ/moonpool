//! AWS DynamoDB metastable failure simulation.
//!
//! Models the October 2025 AWS DynamoDB outage: a DNS race condition triggers
//! congestive collapse in the fleet management layer. The defining characteristic
//! of a metastable failure is that the trigger resolves but the system stays broken.
//!
//! Architecture:
//! - DnsManager: service discovery with latent race condition (trigger)
//! - LeaseStore: capacity-limited lease service (DynamoDB analog)
//! - FleetManager: 16 hosts with autonomous lease renewal loops (DWFM analog)
//! - ClientDriver: traffic generator + metastable failure detector

pub mod dns;
pub mod driver;
pub mod fleet;
pub mod invariants;
pub mod lease_store;

use moonpool_sim::{SimulationBuilder, SimulationReport};

/// Helper to run a simulation and return the report.
pub fn run_simulation(builder: SimulationBuilder) -> SimulationReport {
    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .rng_seed(tokio::runtime::RngSeed::from_bytes(b"deterministic"))
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    local_runtime.block_on(async move { builder.run().await })
}

#[cfg(test)]
mod tests {
    use moonpool_sim::SimulationBuilder;

    use super::dns::DnsWorkload;
    use super::driver::DriverWorkload;
    use super::fleet::FleetWorkload;
    use super::invariants::RecoveryInvariant;
    use super::lease_store::LeaseStoreWorkload;
    use super::run_simulation;

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
}
