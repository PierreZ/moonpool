//! Binary target for the crash-aware journal simulation.
//!
//! One node owns a `moonpool-journal` write-ahead log and is crashed over and
//! over by attrition, on a disk running `Chaos::Storage(Random)`; every boot
//! recovers the journal and checks that no acknowledged entry was lost,
//! changed, or mistaken for corruption.

use std::time::Duration;

use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder, StorageFault,
    StorageFaultMask,
};
use moonpool_sim_examples::journal::{JournalNode, JournalWorkload};
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
                },
                mode: ChaosMode::Random,
            },
            Chaos::Storage(ChaosMode::Random),
        ])
        // The disk's crash physics (lost, latent, shorn sectors), lost
        // directory entries, failed syncs, short transfers and slow-disk
        // episodes are all in a lone journal's contract. Rot, EIO,
        // misdirected and phantom writes damage data a lone node has no
        // second copy of, and a failed disk never answers again: those need
        // replication, so this single node runs without them.
        .storage_fault_mask(
            StorageFaultMask::all()
                .without(StorageFault::Corruption)
                .without(StorageFault::Eio)
                .without(StorageFault::Misdirect)
                .without(StorageFault::PhantomWrite)
                .without(StorageFault::DiskFailure),
        )
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(50)
        .run()
        .expect("simulation configuration is valid");

    finish_or_exit_on_failure(&report);
}
