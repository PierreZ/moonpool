//! Binary target for the journal simulation.
//!
//! One node owns a `moonpool-journal` CLSTORE journal and is crashed over and
//! over by attrition, on two disks in turn (`Disk`):
//!
//! - a faulty disk: `Chaos::Storage(Random)` with every family but a failed
//!   disk, plus aimed double faults. Every boot checks that the journal
//!   returned each acknowledged write or reported it damaged, never forgot
//!   or changed it;
//! - an honest disk: only the families that never change bytes a sync
//!   reported durable. Every boot checks that the journal returned each
//!   acknowledged write, with no damage reported and no open refused.
//!
//! `JOURNAL_SIM_SEEDS` sets another seed count for each disk (the nightly
//! workflow raises it). `JOURNAL_SIM_SEED` replays one seed on both disks.

use std::time::Duration;

use moonpool_sim::{
    Attrition, AttritionScope, AttritionVictims, Chaos, ChaosMode, SimulationBuilder,
    SimulationReport,
};
use moonpool_sim_examples::journal::{Disk, JournalNode, JournalStrike, JournalWorkload};
use moonpool_sim_examples::support::{finish_or_exit_on_failure, finish_or_exit_unless_reached};

/// A kill kept, in the page cache, sectors that a failed writeback left off
/// the device: the state in which a journal that reads the cache at its next
/// open takes a failed commit for a written one (fsyncgate).
const STRANDED_KEPT: &str = "disk: a kill kept sectors that a failed writeback stranded";

/// The number in the environment variable `name`, if it is set. A value
/// that is set but is not a number stops the run: falling back to the
/// default would run other seeds than the ones asked for, in silence.
fn env<T: std::str::FromStr>(name: &str) -> Option<T> {
    let value = std::env::var(name).ok()?;
    if let Ok(number) = value.trim().parse() {
        return Some(number);
    }
    eprintln!("{name}={value:?} is not a number");
    std::process::exit(2);
}

/// The seeds each disk runs: `JOURNAL_SIM_SEEDS`, or `default`.
fn seeds(default: usize) -> usize {
    env("JOURNAL_SIM_SEEDS").unwrap_or(default)
}

/// The run of the node on `disk`, for `iterations` seeds.
fn run(disk: Disk, iterations: usize) -> SimulationReport {
    let builder = SimulationBuilder::new()
        .processes(1, move || Box::new(JournalNode(disk)))
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
            // Spikes the rare rates on some seeds, failed writebacks among
            // them: at the base rate one seed in thirty sees one.
            Chaos::BuggifyKnobs,
        ])
        // Never a failed disk: one never answers again, so a lone node can
        // only stop. See `Disk` for the rest.
        .storage_fault_mask(disk.faults())
        .chaos_duration(Duration::from_secs(30))
        .set_iterations(iterations);
    let builder = match disk {
        // Rot, EIO, misdirected and phantom writes damage data the node has
        // no second copy of, and the strikes aim two faults at one batch:
        // the node checks that the journal reports that damage.
        Disk::Faulty => builder.fault_factory(|| Box::new(JournalStrike)),
        Disk::Honest => builder,
    };
    let builder = match env::<u64>("JOURNAL_SIM_SEED") {
        Some(seed) => builder.set_debug_seeds(vec![seed]).set_iterations(1),
        None => builder,
    };
    builder.run().expect("simulation configuration is valid")
}

fn main() {
    moonpool_sim::init_sim_tracing(tracing::Level::WARN);

    eprintln!("=== journal on a faulty disk ===");
    finish_or_exit_on_failure(&run(Disk::Faulty, seeds(50)));

    // More seeds: a seed is cheap, and the path this disk is for (a failed
    // writeback, then a restart that keeps the page cache, then a power
    // loss) needs three rare things in one seed.
    eprintln!("=== journal on an honest disk ===");
    let honest = run(Disk::Honest, seeds(500));
    finish_or_exit_on_failure(&honest);
    if env::<u64>("JOURNAL_SIM_SEED").is_none() {
        finish_or_exit_unless_reached(&honest, STRANDED_KEPT);
    }
}
