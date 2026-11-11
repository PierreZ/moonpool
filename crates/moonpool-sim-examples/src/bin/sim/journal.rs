//! Binary target for the journal simulation.
//!
//! One node owns a `moonpool-journal` CLSTORE journal and is crashed over and
//! over by attrition, on a disk running `Chaos::Storage(Random)` with every
//! family but a failed disk, plus aimed double faults; every boot checks
//! that the journal returned each acknowledged write or reported it
//! damaged, never forgot or changed it.

use std::time::Duration;

use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder, StorageFault,
    StorageFaultMask,
};
use moonpool_sim_examples::journal::{JournalNode, JournalStrike, JournalWorkload};
use moonpool_sim_examples::support::finish_or_exit_on_failure;

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    let report = SimulationBuilder::new()
        .processes(1, || Box::new(JournalNode))
        .workload(JournalWorkload)
        // Crash-heavy, and never a wipe: a wiped disk is data loss by
        // definition, not something a local journal can recover from.
        .enable_chaos([
            Chaos::Attrition {
                config: Attrition {
                    max_dead: 1,
                    prob_graceful: 0.2,
                    prob_crash: 0.8,
                    prob_wipe: 0.0,
                    recovery_delay_ms: None,
                    grace_period_ms: None,
                    scope: AttritionScope::PerProcess,
                    victims: AttritionVictims::Any,
                    wipe_budget: None,
                },
                mode: ChaosMode::Random,
            },
            Chaos::Storage(ChaosMode::Random),
        ])
        // Every family but a failed disk: one never answers again, so a
        // lone node can only stop. Rot, EIO, misdirected and phantom writes
        // damage data the node has no second copy of; the node checks that
        // the journal reports that damage (the reporting oracle).
        .storage_fault_mask(StorageFaultMask::all().without(StorageFault::DiskFailure))
        .fault_factory(|| Box::new(JournalStrike))
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(50)
        .run()
        .expect("simulation configuration is valid");

    finish_or_exit_on_failure(&report);
}
