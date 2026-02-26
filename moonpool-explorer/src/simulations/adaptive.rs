//! Adaptive exploration scenario functions.
//!
//! These scenarios exercise the adaptive code path (`dispatch_split` ->
//! `adaptive_split_on_discovery`) with coverage-yield-driven batching
//! and 3-level energy budgets.
//!
//! Each scenario provides a minimal xorshift64 RNG via thread-local storage,
//! wired into moonpool-explorer through `set_rng_hooks`. Since scenarios use
//! `fork()`, each must run in its own process (nextest default).

use std::cell::Cell;

use crate::{AdaptiveConfig, ExplorationConfig};

// ---------------------------------------------------------------------------
// Shared xorshift64 RNG infrastructure
// ---------------------------------------------------------------------------

thread_local! {
    /// Current xorshift64 RNG state.
    pub static RNG_STATE: Cell<u64> = const { Cell::new(1) };
    /// Number of RNG calls since last reseed.
    pub static CALL_COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Return the current RNG call count.
pub fn get_count() -> u64 {
    CALL_COUNT.with(|c| c.get())
}

/// Reseed the xorshift64 RNG and reset the call counter.
pub fn reseed(seed: u64) {
    // xorshift64 requires non-zero state
    RNG_STATE.with(|c| c.set(if seed == 0 { 1 } else { seed }));
    CALL_COUNT.with(|c| c.set(0));
}

/// Advance the xorshift64 RNG and return the next value.
pub fn next_random() -> u64 {
    CALL_COUNT.with(|c| c.set(c.get() + 1));
    RNG_STATE.with(|c| {
        let mut s = c.get();
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        c.set(s);
        s
    })
}

/// Return a random integer in `0..divisor`.
pub fn random_below(divisor: u32) -> u32 {
    (next_random() % divisor as u64) as u32
}

// ---------------------------------------------------------------------------
// Scenario 1: Maze cascade
// ---------------------------------------------------------------------------

/// Cascading probability gates with dependent locks.
///
/// 3 locks, each requiring 2 probability gates at P~0.3. Lock dependencies
/// form a chain: lock 1 requires lock 0, lock 2 requires lock 0 + lock 1.
/// Each gate success is a distinct fork point, creating a cascade where
/// adaptive forking amplifies the probability at each level.
///
/// Brute-force probability: (0.3^2)^3 ~ 7x10^-4.
/// With adaptive forking the cascade amplifies through 7 fork points.
pub fn run_adaptive_maze_cascade() -> Result<(), String> {
    crate::set_rng_hooks(get_count, reseed);
    reseed(42);

    crate::init(ExplorationConfig {
        max_depth: 8,
        timelines_per_split: 4,
        global_energy: 150,
        adaptive: Some(AdaptiveConfig {
            batch_size: 4,
            min_timelines: 4,
            max_timelines: 20,
            per_mark_energy: 15,
            warm_min_timelines: None,
        }),
        parallelism: None,
    })
    .map_err(|e| format!("init failed: {e}"))?;

    // Entry gate — always triggers, guarantees the adaptive path fires
    crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "maze_entry");

    // Lock 0: two gates at P~0.3
    let g0a = random_below(10) < 3;
    if g0a {
        crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "maze_g0a");
        let g0b = random_below(10) < 3;
        if g0b {
            crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "maze_lock0");

            // Lock 1: requires lock 0, two gates at P~0.3
            let g1a = random_below(10) < 3;
            if g1a {
                crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "maze_g1a");
                let g1b = random_below(10) < 3;
                if g1b {
                    crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "maze_lock1");

                    // Lock 2: requires lock 0 + lock 1, two gates at P~0.3
                    let g2a = random_below(10) < 3;
                    if g2a {
                        crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "maze_g2a");
                        let g2b = random_below(10) < 3;
                        if g2b {
                            crate::assertion_bool(
                                crate::AssertKind::Sometimes,
                                true,
                                true,
                                "maze_lock2",
                            );

                            // Bug: all 3 locks open!
                            if crate::explorer_is_child() {
                                crate::exit_child(42);
                            }
                        }
                    }
                }
            }
        }
    }

    // Children must exit to avoid running parent assertions
    if crate::explorer_is_child() {
        crate::exit_child(0);
    }

    // Parent: read stats before cleanup frees shared memory
    let stats =
        crate::get_exploration_stats().ok_or_else(|| "stats should be available".to_string())?;
    crate::cleanup();

    if stats.total_timelines == 0 {
        return Err(format!(
            "expected forked children, got total_timelines={}",
            stats.total_timelines
        ));
    }
    if stats.fork_points == 0 {
        return Err(format!(
            "expected fork points, got fork_points={}",
            stats.fork_points
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 2: Dungeon floors
// ---------------------------------------------------------------------------

/// Progressive multi-floor exploration.
///
/// 5 floors, each with a key gate at P=0.2. Must find key on floor N to
/// attempt floor N+1 (linear chain). Each floor key discovery is a fork
/// point, so the adaptive explorer cascades deeper with each floor reached.
///
/// Brute-force probability: 0.2^5 ~ 3.2x10^-4.
/// Fork cascade amplifies at each floor.
pub fn run_adaptive_dungeon_floors() -> Result<(), String> {
    crate::set_rng_hooks(get_count, reseed);
    reseed(7777);

    crate::init(ExplorationConfig {
        max_depth: 7,
        timelines_per_split: 4,
        global_energy: 200,
        adaptive: Some(AdaptiveConfig {
            batch_size: 4,
            min_timelines: 4,
            max_timelines: 25,
            per_mark_energy: 20,
            warm_min_timelines: None,
        }),
        parallelism: None,
    })
    .map_err(|e| format!("init failed: {e}"))?;

    // Entry — always triggers, starts the exploration
    crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "dungeon_entry");

    let mut floors_cleared = 0u32;

    for floor in 0..5 {
        // Key gate: P=0.2
        let has_key = random_below(5) == 0;
        if !has_key {
            break;
        }

        // Each floor gets its own fork point name
        let name = match floor {
            0 => "dungeon_f0",
            1 => "dungeon_f1",
            2 => "dungeon_f2",
            3 => "dungeon_f3",
            4 => "dungeon_f4",
            _ => break,
        };
        crate::assertion_bool(crate::AssertKind::Sometimes, true, true, name);
        floors_cleared = floor + 1;
    }

    // Treasure found: all 5 keys collected
    if floors_cleared == 5 && crate::explorer_is_child() {
        crate::exit_child(42);
    }

    if crate::explorer_is_child() {
        crate::exit_child(0);
    }

    let stats =
        crate::get_exploration_stats().ok_or_else(|| "stats should be available".to_string())?;
    crate::cleanup();

    if stats.total_timelines == 0 {
        return Err(format!(
            "expected forked children, got total_timelines={}",
            stats.total_timelines
        ));
    }
    if stats.fork_points == 0 {
        return Err(format!(
            "expected fork points, got fork_points={}",
            stats.fork_points
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 3: Energy budget
// ---------------------------------------------------------------------------

/// Verify that the 3-level energy budget (global, per-mark, realloc pool)
/// constrains adaptive forking. Uses low energy values to ensure bounds.
///
/// 3 always-true gates maximize energy consumption. The global energy cap
/// of 8 limits total forks regardless of per-mark budgets.
pub fn run_adaptive_energy_budget() -> Result<(), String> {
    crate::set_rng_hooks(get_count, reseed);
    reseed(99);

    crate::init(ExplorationConfig {
        max_depth: 3,
        timelines_per_split: 4,
        global_energy: 8,
        adaptive: Some(AdaptiveConfig {
            batch_size: 2,
            min_timelines: 2,
            max_timelines: 6,
            per_mark_energy: 3,
            warm_min_timelines: None,
        }),
        parallelism: None,
    })
    .map_err(|e| format!("init failed: {e}"))?;

    // All gates always fire — maximizes energy consumption
    crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "energy_a");
    crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "energy_b");
    crate::assertion_bool(crate::AssertKind::Sometimes, true, true, "energy_c");

    if crate::explorer_is_child() {
        crate::exit_child(0);
    }

    let stats =
        crate::get_exploration_stats().ok_or_else(|| "stats should be available".to_string())?;
    crate::cleanup();

    if stats.total_timelines > 8 {
        return Err(format!(
            "energy limit exceeded: total_timelines={} (expected <= 8)",
            stats.total_timelines
        ));
    }
    if stats.global_energy < 0 {
        return Err(format!(
            "energy went negative: global_energy={}",
            stats.global_energy
        ));
    }
    if stats.realloc_pool_remaining < 0 {
        return Err(format!(
            "realloc pool went negative: realloc_pool={}",
            stats.realloc_pool_remaining
        ));
    }

    Ok(())
}
