//! Binary target for banking chaos simulation.
//!
//! Note: This simulation has a known transfer revert bug (see TODO).

use moonpool::simulations::banking::{
    invariants::{ConservationLaw, NonNegativeBalances},
    workloads::BankingWorkload,
};
use moonpool_sim::SimulationBuilder;

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    eprintln!("WARNING: Banking chaos simulation has a known transfer revert bug.");
    eprintln!("Running anyway for coverage data...");

    let names = ["alice", "bob", "charlie", "dave", "eve"];
    let accounts: Vec<String> = names.iter().map(|s| s.to_string()).collect();

    let local_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build_local(Default::default())
        .expect("Failed to build local runtime");

    let report = local_runtime.block_on(async move {
        SimulationBuilder::new()
            .workload(BankingWorkload::new(200, accounts))
            .invariant(ConservationLaw)
            .invariant(NonNegativeBalances)
            .set_iterations(100)
            .run()
            .await
    });

    report.eprint();
}
