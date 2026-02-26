//! Actor simulation test suite.
//!
//! FDB-style alphabet workloads for the virtual actor system:
//! conservation laws, lifecycle verification, and chaos testing.

pub mod invariants;
pub mod operations;
pub mod workloads;

#[cfg(test)]
mod tests {
    use moonpool_sim::SimulationBuilder;

    use super::invariants::{ConservationLaw, NonNegativeBalances};
    use super::workloads::BankingWorkload;

    fn account_names(n: usize) -> Vec<String> {
        let names = ["alice", "bob", "charlie", "dave", "eve"];
        names.iter().take(n).map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_banking_basic() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .workload(BankingWorkload::new(50, account_names(3)))
                .invariant(ConservationLaw)
                .invariant(NonNegativeBalances)
                .set_iterations(5)
                .run()
                .await
        });

        assert_eq!(
            report.failed_runs, 0,
            "banking basic had failures: seeds_failing={:?}",
            report.seeds_failing
        );
        assert!(
            report.successful_runs > 0,
            "no successful runs in banking basic"
        );
    }

    #[test]
    fn test_banking_transfer_heavy() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .workload(BankingWorkload::new(100, account_names(5)))
                .invariant(ConservationLaw)
                .invariant(NonNegativeBalances)
                .set_iterations(10)
                .run()
                .await
        });

        assert_eq!(
            report.failed_runs, 0,
            "banking transfer heavy had failures: seeds_failing={:?}",
            report.seeds_failing
        );
    }

    #[test]
    #[ignore] // TODO: fix transfer revert bug -- model/actor divergence on failed deposit after withdraw
    fn slow_simulation_banking_chaos() {
        let local_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build_local(Default::default())
            .expect("Failed to build local runtime");

        let report = local_runtime.block_on(async move {
            SimulationBuilder::new()
                .workload(BankingWorkload::new(200, account_names(5)))
                .invariant(ConservationLaw)
                .invariant(NonNegativeBalances)
                .set_iterations(100)
                .run()
                .await
        });

        assert_eq!(
            report.failed_runs, 0,
            "slow simulation banking chaos had failures: seeds_failing={:?}",
            report.seeds_failing
        );
    }
}
