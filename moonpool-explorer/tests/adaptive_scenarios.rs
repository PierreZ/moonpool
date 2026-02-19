//! Integration tests for adaptive fork-based exploration.
//!
//! These tests exercise the adaptive code path (`dispatch_split →
//! `adaptive_split_on_discovery`) with coverage-yield-driven batching
//! and 3-level energy budgets.
//!
//! Each test provides a minimal xorshift64 RNG via thread-local storage,
//! wired into moonpool-explorer through `set_rng_hooks`. Since tests use
//! `fork()`, each must run in its own process (nextest default).

use std::cell::Cell;

use moonpool_explorer::{AdaptiveConfig, ExplorationConfig};

// ---------------------------------------------------------------------------
// Shared xorshift64 RNG infrastructure
// ---------------------------------------------------------------------------

thread_local! {
    static RNG_STATE: Cell<u64> = const { Cell::new(1) };
    static CALL_COUNT: Cell<u64> = const { Cell::new(0) };
}

fn get_count() -> u64 {
    CALL_COUNT.with(|c| c.get())
}

fn reseed(seed: u64) {
    // xorshift64 requires non-zero state
    RNG_STATE.with(|c| c.set(if seed == 0 { 1 } else { seed }));
    CALL_COUNT.with(|c| c.set(0));
}

/// Advance the xorshift64 RNG and return the next value.
fn next_random() -> u64 {
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
fn random_below(divisor: u32) -> u32 {
    (next_random() % divisor as u64) as u32
}

// ---------------------------------------------------------------------------
// Test 1: Maze cascade — cascading probability gates with dependent locks
// ---------------------------------------------------------------------------

/// Backport of the standalone maze scenario.
///
/// 3 locks, each requiring 2 probability gates at P≈0.3. Lock dependencies
/// form a chain: lock 1 requires lock 0, lock 2 requires lock 0 + lock 1.
/// Each gate success is a distinct fork point, creating a cascade where
/// adaptive forking amplifies the probability at each level.
///
/// Brute-force probability: (0.3²)³ ≈ 7×10⁻⁴.
/// With adaptive forking the cascade amplifies through 7 fork points.
#[test]
fn slow_simulation_adaptive_maze_cascade() {
    moonpool_explorer::set_rng_hooks(get_count, reseed);
    reseed(42);

    moonpool_explorer::init(ExplorationConfig {
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
    .expect("init failed");

    // Entry gate — always triggers, guarantees the adaptive path fires
    moonpool_explorer::assertion_bool(
        moonpool_explorer::AssertKind::Sometimes,
        true,
        true,
        "maze_entry",
    );

    // Lock 0: two gates at P≈0.3
    let g0a = random_below(10) < 3;
    if g0a {
        moonpool_explorer::assertion_bool(
            moonpool_explorer::AssertKind::Sometimes,
            true,
            true,
            "maze_g0a",
        );
        let g0b = random_below(10) < 3;
        if g0b {
            moonpool_explorer::assertion_bool(
                moonpool_explorer::AssertKind::Sometimes,
                true,
                true,
                "maze_lock0",
            );

            // Lock 1: requires lock 0, two gates at P≈0.3
            let g1a = random_below(10) < 3;
            if g1a {
                moonpool_explorer::assertion_bool(
                    moonpool_explorer::AssertKind::Sometimes,
                    true,
                    true,
                    "maze_g1a",
                );
                let g1b = random_below(10) < 3;
                if g1b {
                    moonpool_explorer::assertion_bool(
                        moonpool_explorer::AssertKind::Sometimes,
                        true,
                        true,
                        "maze_lock1",
                    );

                    // Lock 2: requires lock 0 + lock 1, two gates at P≈0.3
                    let g2a = random_below(10) < 3;
                    if g2a {
                        moonpool_explorer::assertion_bool(
                            moonpool_explorer::AssertKind::Sometimes,
                            true,
                            true,
                            "maze_g2a",
                        );
                        let g2b = random_below(10) < 3;
                        if g2b {
                            moonpool_explorer::assertion_bool(
                                moonpool_explorer::AssertKind::Sometimes,
                                true,
                                true,
                                "maze_lock2",
                            );

                            // Bug: all 3 locks open!
                            if moonpool_explorer::explorer_is_child() {
                                moonpool_explorer::exit_child(42);
                            }
                        }
                    }
                }
            }
        }
    }

    // Children must exit to avoid running parent assertions
    if moonpool_explorer::explorer_is_child() {
        moonpool_explorer::exit_child(0);
    }

    // Parent: read stats before cleanup frees shared memory
    let stats = moonpool_explorer::get_exploration_stats().expect("stats should be available");
    moonpool_explorer::cleanup();

    assert!(
        stats.total_timelines > 0,
        "expected forked children, got total_timelines={}",
        stats.total_timelines
    );
    assert!(
        stats.fork_points > 0,
        "expected fork points, got fork_points={}",
        stats.fork_points
    );
    // Note: stats.global_energy reflects SharedStats (fixed-count path).
    // In adaptive mode, energy is tracked in EnergyBudget, so we verify
    // consumption indirectly via total_timelines > 0 above.
}

// ---------------------------------------------------------------------------
// Test 2: Dungeon floors — progressive multi-floor exploration
// ---------------------------------------------------------------------------

/// Backport of the standalone dungeon scenario.
///
/// 5 floors, each with a key gate at P=0.2. Must find key on floor N to
/// attempt floor N+1 (linear chain). Each floor key discovery is a fork
/// point, so the adaptive explorer cascades deeper with each floor reached.
///
/// Brute-force probability: 0.2⁵ ≈ 3.2×10⁻⁴.
/// Fork cascade amplifies at each floor.
#[test]
fn slow_simulation_adaptive_dungeon_floors() {
    moonpool_explorer::set_rng_hooks(get_count, reseed);
    reseed(7777);

    moonpool_explorer::init(ExplorationConfig {
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
    .expect("init failed");

    // Entry — always triggers, starts the exploration
    moonpool_explorer::assertion_bool(
        moonpool_explorer::AssertKind::Sometimes,
        true,
        true,
        "dungeon_entry",
    );

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
        moonpool_explorer::assertion_bool(
            moonpool_explorer::AssertKind::Sometimes,
            true,
            true,
            name,
        );
        floors_cleared = floor + 1;
    }

    // Treasure found: all 5 keys collected
    if floors_cleared == 5 && moonpool_explorer::explorer_is_child() {
        moonpool_explorer::exit_child(42);
    }

    if moonpool_explorer::explorer_is_child() {
        moonpool_explorer::exit_child(0);
    }

    let stats = moonpool_explorer::get_exploration_stats().expect("stats should be available");
    moonpool_explorer::cleanup();

    assert!(
        stats.total_timelines > 0,
        "expected forked children, got total_timelines={}",
        stats.total_timelines
    );
    assert!(
        stats.fork_points > 0,
        "expected fork points, got fork_points={}",
        stats.fork_points
    );
}

// ---------------------------------------------------------------------------
// Test 3: Energy budget — verify 3-level energy constraints
// ---------------------------------------------------------------------------

/// Verify that the 3-level energy budget (global, per-mark, realloc pool)
/// constrains adaptive forking. Uses low energy values to ensure bounds.
///
/// 3 always-true gates maximize energy consumption. The global energy cap
/// of 8 limits total forks regardless of per-mark budgets.
#[test]
fn slow_simulation_adaptive_energy_budget() {
    moonpool_explorer::set_rng_hooks(get_count, reseed);
    reseed(99);

    moonpool_explorer::init(ExplorationConfig {
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
    .expect("init failed");

    // All gates always fire — maximizes energy consumption
    moonpool_explorer::assertion_bool(
        moonpool_explorer::AssertKind::Sometimes,
        true,
        true,
        "energy_a",
    );
    moonpool_explorer::assertion_bool(
        moonpool_explorer::AssertKind::Sometimes,
        true,
        true,
        "energy_b",
    );
    moonpool_explorer::assertion_bool(
        moonpool_explorer::AssertKind::Sometimes,
        true,
        true,
        "energy_c",
    );

    if moonpool_explorer::explorer_is_child() {
        moonpool_explorer::exit_child(0);
    }

    let stats = moonpool_explorer::get_exploration_stats().expect("stats should be available");
    moonpool_explorer::cleanup();

    assert!(
        stats.total_timelines <= 8,
        "energy limit exceeded: total_timelines={} (expected <= 8)",
        stats.total_timelines
    );
    assert!(
        stats.global_energy >= 0,
        "energy went negative: global_energy={}",
        stats.global_energy
    );
    assert!(
        stats.realloc_pool_remaining >= 0,
        "realloc pool went negative: realloc_pool={}",
        stats.realloc_pool_remaining
    );
}
