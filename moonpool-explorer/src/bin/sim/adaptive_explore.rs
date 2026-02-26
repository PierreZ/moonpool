//! Binary target for adaptive exploration scenarios.
//!
//! Runs all adaptive exploration scenarios sequentially.
//! Exit code 1 on any failure.

use std::process;

fn main() {
    eprintln!("=== Adaptive Maze Cascade ===");
    if let Err(e) = moonpool_explorer::simulations::adaptive::run_adaptive_maze_cascade() {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    eprintln!("=== Adaptive Dungeon Floors ===");
    if let Err(e) = moonpool_explorer::simulations::adaptive::run_adaptive_dungeon_floors() {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    eprintln!("=== Adaptive Energy Budget ===");
    if let Err(e) = moonpool_explorer::simulations::adaptive::run_adaptive_energy_budget() {
        eprintln!("FAILED: {e}");
        process::exit(1);
    }
    eprintln!("PASSED\n");

    eprintln!("All adaptive scenarios passed.");
}
