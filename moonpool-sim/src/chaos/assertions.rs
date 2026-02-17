//! Antithesis-style assertion macros and result tracking for simulation testing.
//!
//! This module provides 14+ assertion macros for testing distributed system
//! properties. Assertions are tracked in shared memory via moonpool-explorer,
//! enabling cross-process tracking across forked exploration timelines.
//!
//! Following the Antithesis principle: **assertions never crash your program**.
//! Always-type assertions log violations at ERROR level and record them via a
//! thread-local flag, allowing the simulation to continue running and discover
//! cascading failures. The simulation runner checks `has_always_violations()`
//! after each iteration to report failures through the normal result pipeline.
//!
//! # Assertion Kinds
//!
//! | Macro | Tracks | Panics | Forks |
//! |-------|--------|--------|-------|
//! | `assert_always!` | yes | no | no |
//! | `assert_always_or_unreachable!` | yes | no | no |
//! | `assert_sometimes!` | yes | no | on first success |
//! | `assert_reachable!` | yes | no | on first reach |
//! | `assert_unreachable!` | yes | no | no |
//! | `assert_always_greater_than!` | yes | no | no |
//! | `assert_always_greater_than_or_equal_to!` | yes | no | no |
//! | `assert_always_less_than!` | yes | no | no |
//! | `assert_always_less_than_or_equal_to!` | yes | no | no |
//! | `assert_sometimes_greater_than!` | yes | no | on watermark improvement |
//! | `assert_sometimes_greater_than_or_equal_to!` | yes | no | on watermark improvement |
//! | `assert_sometimes_less_than!` | yes | no | on watermark improvement |
//! | `assert_sometimes_less_than_or_equal_to!` | yes | no | on watermark improvement |
//! | `assert_sometimes_all!` | yes | no | on frontier advance |
//! | `assert_sometimes_each!` | yes | no | on discovery/quality |

use std::cell::Cell;
use std::collections::HashMap;

// =============================================================================
// Thread-local violation tracking (Antithesis-style: never panic)
// =============================================================================

thread_local! {
    static ALWAYS_VIOLATION_COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Record that an always-type assertion was violated during this iteration.
///
/// Called by always-type macros instead of panicking. The simulation runner
/// checks `has_always_violations()` after each iteration to report failures.
pub fn record_always_violation() {
    ALWAYS_VIOLATION_COUNT.with(|c| c.set(c.get() + 1));
}

/// Reset the violation counter. Must be called at the start of each iteration.
pub fn reset_always_violations() {
    ALWAYS_VIOLATION_COUNT.with(|c| c.set(0));
}

/// Check whether any always-type assertion was violated during this iteration.
pub fn has_always_violations() -> bool {
    ALWAYS_VIOLATION_COUNT.with(|c| c.get() > 0)
}

/// Statistics for a tracked assertion.
///
/// Records the total number of times an assertion was checked and how many
/// times it succeeded, enabling calculation of success rates for probabilistic
/// properties in distributed systems.
#[derive(Debug, Clone, PartialEq)]
pub struct AssertionStats {
    /// Total number of times this assertion was evaluated
    pub total_checks: usize,
    /// Number of times the assertion condition was true
    pub successes: usize,
}

impl AssertionStats {
    /// Create new assertion statistics starting at zero.
    pub fn new() -> Self {
        Self {
            total_checks: 0,
            successes: 0,
        }
    }

    /// Calculate the success rate as a percentage (0.0 to 100.0).
    ///
    /// Returns 0.0 if no checks have been performed yet.
    pub fn success_rate(&self) -> f64 {
        if self.total_checks == 0 {
            0.0
        } else {
            (self.successes as f64 / self.total_checks as f64) * 100.0
        }
    }

    /// Record a new assertion check with the given result.
    ///
    /// Increments total_checks and successes (if the result was true).
    pub fn record(&mut self, success: bool) {
        self.total_checks += 1;
        if success {
            self.successes += 1;
        }
    }
}

impl Default for AssertionStats {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Thin backing wrappers (for $crate:: macro hygiene)
// =============================================================================

/// Boolean assertion backing wrapper.
///
/// Delegates to `moonpool_explorer::assertion_bool`.
/// Accepts both `&str` and `String` message arguments.
pub fn on_assertion_bool(
    msg: impl AsRef<str>,
    condition: bool,
    kind: moonpool_explorer::AssertKind,
    must_hit: bool,
) {
    moonpool_explorer::assertion_bool(kind, must_hit, condition, msg.as_ref());
}

/// Numeric assertion backing wrapper.
///
/// Delegates to `moonpool_explorer::assertion_numeric`.
/// Accepts both `&str` and `String` message arguments.
pub fn on_assertion_numeric(
    msg: impl AsRef<str>,
    value: i64,
    cmp: moonpool_explorer::AssertCmp,
    threshold: i64,
    kind: moonpool_explorer::AssertKind,
    maximize: bool,
) {
    moonpool_explorer::assertion_numeric(kind, cmp, maximize, value, threshold, msg.as_ref());
}

/// Compound boolean assertion backing wrapper.
///
/// Delegates to `moonpool_explorer::assertion_sometimes_all`.
pub fn on_assertion_sometimes_all(msg: impl AsRef<str>, named_bools: &[(&str, bool)]) {
    moonpool_explorer::assertion_sometimes_all(msg.as_ref(), named_bools);
}

/// Notify the exploration framework of a per-value bucketed assertion.
///
/// This delegates to moonpool-explorer's EachBucket infrastructure for
/// fork-based exploration with identity keys and quality watermarks.
pub fn on_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    moonpool_explorer::assertion_sometimes_each(msg, keys, quality);
}

// =============================================================================
// Shared-memory-based result collection
// =============================================================================

/// Get current assertion statistics for all tracked assertions.
///
/// Reads from shared memory assertion slots. Returns a snapshot of assertion
/// results for reporting and validation.
pub fn get_assertion_results() -> HashMap<String, AssertionStats> {
    let slots = moonpool_explorer::assertion_read_all();
    let mut results = HashMap::new();

    for slot in &slots {
        let total = slot.pass_count.saturating_add(slot.fail_count) as usize;
        if total == 0 {
            continue;
        }
        results.insert(
            slot.msg.clone(),
            AssertionStats {
                total_checks: total,
                successes: slot.pass_count as usize,
            },
        );
    }

    results
}

/// Reset all assertion statistics.
///
/// Zeros the shared memory assertion table. Should be called before each
/// simulation run to ensure clean state between consecutive simulations.
pub fn reset_assertion_results() {
    moonpool_explorer::reset_assertions();
}

/// Check assertion validation and panic if violations are found.
///
/// Checks all assertion kinds for their specific violation conditions.
///
/// # Panics
///
/// Panics if there are assertion violations.
pub fn panic_on_assertion_violations(_report: &crate::runner::SimulationReport) {
    let violations = validate_assertion_contracts();

    if !violations.is_empty() {
        println!("Assertion violations found:");
        for violation in &violations {
            println!("  - {}", violation);
        }
        panic!("Unexpected assertion violations detected!");
    } else {
        println!("All assertions passed validation!");
    }
}

/// Validate all assertion contracts based on their kind.
///
/// Per-kind validation:
/// - Always: violation if fail_count > 0, or if never reached (must_hit + total == 0)
/// - AlwaysOrUnreachable: violation if fail_count > 0
/// - Sometimes: violation if pass_count == 0 when total > 0
/// - Reachable: violation if never reached (pass_count == 0)
/// - Unreachable: violation if ever reached (pass_count > 0)
/// - NumericAlways: violation if fail_count > 0
/// - NumericSometimes: violation if pass_count == 0 when total > 0
///
/// # Returns
///
/// A vector of violation messages, or empty if all assertions are valid.
pub fn validate_assertion_contracts() -> Vec<String> {
    let mut violations = Vec::new();
    let slots = moonpool_explorer::assertion_read_all();

    for slot in &slots {
        let total = slot.pass_count.saturating_add(slot.fail_count);
        let kind = moonpool_explorer::AssertKind::from_u8(slot.kind);

        match kind {
            Some(moonpool_explorer::AssertKind::Always) => {
                if slot.fail_count > 0 {
                    violations.push(format!(
                        "assert_always!('{}') failed {} times out of {}",
                        slot.msg, slot.fail_count, total
                    ));
                }
                if slot.must_hit != 0 && total == 0 {
                    violations.push(format!("assert_always!('{}') was never reached", slot.msg));
                }
            }
            Some(moonpool_explorer::AssertKind::AlwaysOrUnreachable) => {
                if slot.fail_count > 0 {
                    violations.push(format!(
                        "assert_always_or_unreachable!('{}') failed {} times out of {}",
                        slot.msg, slot.fail_count, total
                    ));
                }
            }
            Some(moonpool_explorer::AssertKind::Sometimes) => {
                if total > 0 && slot.pass_count == 0 {
                    violations.push(format!(
                        "assert_sometimes!('{}') has 0% success rate ({} checks)",
                        slot.msg, total
                    ));
                }
            }
            Some(moonpool_explorer::AssertKind::Reachable) => {
                if slot.pass_count == 0 {
                    violations.push(format!(
                        "assert_reachable!('{}') was never reached",
                        slot.msg
                    ));
                }
            }
            Some(moonpool_explorer::AssertKind::Unreachable) => {
                if slot.pass_count > 0 {
                    violations.push(format!(
                        "assert_unreachable!('{}') was reached {} times",
                        slot.msg, slot.pass_count
                    ));
                }
            }
            Some(moonpool_explorer::AssertKind::NumericAlways) => {
                if slot.fail_count > 0 {
                    violations.push(format!(
                        "numeric assert_always ('{}') failed {} times out of {}",
                        slot.msg, slot.fail_count, total
                    ));
                }
            }
            Some(moonpool_explorer::AssertKind::NumericSometimes) => {
                if total > 0 && slot.pass_count == 0 {
                    violations.push(format!(
                        "numeric assert_sometimes ('{}') has 0% success rate ({} checks)",
                        slot.msg, total
                    ));
                }
            }
            Some(moonpool_explorer::AssertKind::BooleanSometimesAll) | None => {
                // BooleanSometimesAll: no simple pass/fail violation contract
                // (the frontier tracking is the guidance mechanism)
            }
        }
    }

    violations
}

// =============================================================================
// Assertion Macros
// =============================================================================

/// Assert that a condition is always true.
///
/// Tracks pass/fail in shared memory for cross-process visibility.
/// Does **not** panic — records the violation via `record_always_violation()`
/// and logs at ERROR level with the seed, following the Antithesis principle
/// that assertions never crash the program.
#[macro_export]
macro_rules! assert_always {
    ($condition:expr, $message:expr) => {
        let __msg = $message;
        let cond = $condition;
        $crate::chaos::assertions::on_assertion_bool(
            &__msg,
            cond,
            $crate::chaos::assertions::_re_export::AssertKind::Always,
            true,
        );
        if !cond {
            let seed = $crate::sim::get_current_sim_seed();
            tracing::error!("[ALWAYS FAILED] seed={} — {}", seed, __msg);
            $crate::chaos::assertions::record_always_violation();
        }
    };
}

/// Assert that a condition is always true when reached, but the code path
/// need not be reached. Does not panic if never evaluated.
///
/// Does **not** panic on failure — records the violation and logs at ERROR level.
#[macro_export]
macro_rules! assert_always_or_unreachable {
    ($condition:expr, $message:expr) => {
        let __msg = $message;
        let cond = $condition;
        $crate::chaos::assertions::on_assertion_bool(
            &__msg,
            cond,
            $crate::chaos::assertions::_re_export::AssertKind::AlwaysOrUnreachable,
            false,
        );
        if !cond {
            let seed = $crate::sim::get_current_sim_seed();
            tracing::error!("[ALWAYS_OR_UNREACHABLE FAILED] seed={} — {}", seed, __msg);
            $crate::chaos::assertions::record_always_violation();
        }
    };
}

/// Assert a condition that should sometimes be true, tracking stats and triggering exploration.
///
/// Does not panic. On first success, triggers a fork to explore alternate timelines.
#[macro_export]
macro_rules! assert_sometimes {
    ($condition:expr, $message:expr) => {
        $crate::chaos::assertions::on_assertion_bool(
            &$message,
            $condition,
            $crate::chaos::assertions::_re_export::AssertKind::Sometimes,
            true,
        );
    };
}

/// Assert that a code path is reachable (should be reached at least once).
///
/// Does not panic. On first reach, triggers a fork.
#[macro_export]
macro_rules! assert_reachable {
    ($message:expr) => {
        $crate::chaos::assertions::on_assertion_bool(
            &$message,
            true,
            $crate::chaos::assertions::_re_export::AssertKind::Reachable,
            true,
        );
    };
}

/// Assert that a code path should never be reached.
///
/// Does **not** panic — records the violation and logs at ERROR level.
/// Tracks in shared memory for reporting.
#[macro_export]
macro_rules! assert_unreachable {
    ($message:expr) => {
        let __msg = $message;
        $crate::chaos::assertions::on_assertion_bool(
            &__msg,
            true,
            $crate::chaos::assertions::_re_export::AssertKind::Unreachable,
            false,
        );
        let seed = $crate::sim::get_current_sim_seed();
        tracing::error!("[UNREACHABLE REACHED] seed={} — {}", seed, __msg);
        $crate::chaos::assertions::record_always_violation();
    };
}

/// Assert that `val > threshold` always holds.
///
/// Does **not** panic on failure — records the violation and logs at ERROR level.
#[macro_export]
macro_rules! assert_always_greater_than {
    ($val:expr, $thresh:expr, $message:expr) => {
        let __msg = $message;
        let __v = $val as i64;
        let __t = $thresh as i64;
        $crate::chaos::assertions::on_assertion_numeric(
            &__msg,
            __v,
            $crate::chaos::assertions::_re_export::AssertCmp::Gt,
            __t,
            $crate::chaos::assertions::_re_export::AssertKind::NumericAlways,
            false,
        );
        if !(__v > __t) {
            let seed = $crate::sim::get_current_sim_seed();
            tracing::error!(
                "[NUMERIC ALWAYS FAILED] seed={} — {} (val={}, thresh={})",
                seed,
                __msg,
                __v,
                __t
            );
            $crate::chaos::assertions::record_always_violation();
        }
    };
}

/// Assert that `val >= threshold` always holds.
///
/// Does **not** panic on failure — records the violation and logs at ERROR level.
#[macro_export]
macro_rules! assert_always_greater_than_or_equal_to {
    ($val:expr, $thresh:expr, $message:expr) => {
        let __msg = $message;
        let __v = $val as i64;
        let __t = $thresh as i64;
        $crate::chaos::assertions::on_assertion_numeric(
            &__msg,
            __v,
            $crate::chaos::assertions::_re_export::AssertCmp::Ge,
            __t,
            $crate::chaos::assertions::_re_export::AssertKind::NumericAlways,
            false,
        );
        if !(__v >= __t) {
            let seed = $crate::sim::get_current_sim_seed();
            tracing::error!(
                "[NUMERIC ALWAYS FAILED] seed={} — {} (val={}, thresh={})",
                seed,
                __msg,
                __v,
                __t
            );
            $crate::chaos::assertions::record_always_violation();
        }
    };
}

/// Assert that `val < threshold` always holds.
///
/// Does **not** panic on failure — records the violation and logs at ERROR level.
#[macro_export]
macro_rules! assert_always_less_than {
    ($val:expr, $thresh:expr, $message:expr) => {
        let __msg = $message;
        let __v = $val as i64;
        let __t = $thresh as i64;
        $crate::chaos::assertions::on_assertion_numeric(
            &__msg,
            __v,
            $crate::chaos::assertions::_re_export::AssertCmp::Lt,
            __t,
            $crate::chaos::assertions::_re_export::AssertKind::NumericAlways,
            true,
        );
        if !(__v < __t) {
            let seed = $crate::sim::get_current_sim_seed();
            tracing::error!(
                "[NUMERIC ALWAYS FAILED] seed={} — {} (val={}, thresh={})",
                seed,
                __msg,
                __v,
                __t
            );
            $crate::chaos::assertions::record_always_violation();
        }
    };
}

/// Assert that `val <= threshold` always holds.
///
/// Does **not** panic on failure — records the violation and logs at ERROR level.
#[macro_export]
macro_rules! assert_always_less_than_or_equal_to {
    ($val:expr, $thresh:expr, $message:expr) => {
        let __msg = $message;
        let __v = $val as i64;
        let __t = $thresh as i64;
        $crate::chaos::assertions::on_assertion_numeric(
            &__msg,
            __v,
            $crate::chaos::assertions::_re_export::AssertCmp::Le,
            __t,
            $crate::chaos::assertions::_re_export::AssertKind::NumericAlways,
            true,
        );
        if !(__v <= __t) {
            let seed = $crate::sim::get_current_sim_seed();
            tracing::error!(
                "[NUMERIC ALWAYS FAILED] seed={} — {} (val={}, thresh={})",
                seed,
                __msg,
                __v,
                __t
            );
            $crate::chaos::assertions::record_always_violation();
        }
    };
}

/// Assert that `val > threshold` sometimes holds. Forks on watermark improvement.
#[macro_export]
macro_rules! assert_sometimes_greater_than {
    ($val:expr, $thresh:expr, $message:expr) => {
        $crate::chaos::assertions::on_assertion_numeric(
            &$message,
            $val as i64,
            $crate::chaos::assertions::_re_export::AssertCmp::Gt,
            $thresh as i64,
            $crate::chaos::assertions::_re_export::AssertKind::NumericSometimes,
            true,
        );
    };
}

/// Assert that `val >= threshold` sometimes holds. Forks on watermark improvement.
#[macro_export]
macro_rules! assert_sometimes_greater_than_or_equal_to {
    ($val:expr, $thresh:expr, $message:expr) => {
        $crate::chaos::assertions::on_assertion_numeric(
            &$message,
            $val as i64,
            $crate::chaos::assertions::_re_export::AssertCmp::Ge,
            $thresh as i64,
            $crate::chaos::assertions::_re_export::AssertKind::NumericSometimes,
            true,
        );
    };
}

/// Assert that `val < threshold` sometimes holds. Forks on watermark improvement.
#[macro_export]
macro_rules! assert_sometimes_less_than {
    ($val:expr, $thresh:expr, $message:expr) => {
        $crate::chaos::assertions::on_assertion_numeric(
            &$message,
            $val as i64,
            $crate::chaos::assertions::_re_export::AssertCmp::Lt,
            $thresh as i64,
            $crate::chaos::assertions::_re_export::AssertKind::NumericSometimes,
            false,
        );
    };
}

/// Assert that `val <= threshold` sometimes holds. Forks on watermark improvement.
#[macro_export]
macro_rules! assert_sometimes_less_than_or_equal_to {
    ($val:expr, $thresh:expr, $message:expr) => {
        $crate::chaos::assertions::on_assertion_numeric(
            &$message,
            $val as i64,
            $crate::chaos::assertions::_re_export::AssertCmp::Le,
            $thresh as i64,
            $crate::chaos::assertions::_re_export::AssertKind::NumericSometimes,
            false,
        );
    };
}

/// Compound boolean assertion: all named bools should sometimes be true simultaneously.
///
/// Tracks a frontier (max number of simultaneously true bools). Forks when
/// the frontier advances.
///
/// # Usage
///
/// ```ignore
/// assert_sometimes_all!("all_nodes_healthy", [
///     ("node_a", node_a_healthy),
///     ("node_b", node_b_healthy),
///     ("node_c", node_c_healthy),
/// ]);
/// ```
#[macro_export]
macro_rules! assert_sometimes_all {
    ($msg:expr, [ $(($name:expr, $val:expr)),+ $(,)? ]) => {
        $crate::chaos::assertions::on_assertion_sometimes_all($msg, &[ $(($name, $val)),+ ])
    };
}

/// Per-value bucketed sometimes assertion with optional quality watermarks.
///
/// Each unique combination of identity keys gets its own bucket. On first
/// discovery of a new bucket, a fork is triggered for exploration. If quality
/// keys are provided, re-forks when quality improves.
///
/// # Usage
///
/// ```ignore
/// // Identity keys only
/// assert_sometimes_each!("gate", [("lock", lock_id), ("depth", depth)]);
///
/// // With quality watermarks
/// assert_sometimes_each!("descended", [("to_floor", floor)], [("health", hp)]);
/// ```
#[macro_export]
macro_rules! assert_sometimes_each {
    ($msg:expr, [ $(($name:expr, $val:expr)),+ $(,)? ]) => {
        $crate::chaos::assertions::on_sometimes_each($msg, &[ $(($name, $val as i64)),+ ], &[])
    };
    ($msg:expr, [ $(($name:expr, $val:expr)),+ $(,)? ], [ $(($qname:expr, $qval:expr)),+ $(,)? ]) => {
        $crate::chaos::assertions::on_sometimes_each(
            $msg,
            &[ $(($name, $val as i64)),+ ],
            &[ $(($qname, $qval as i64)),+ ],
        )
    };
}

/// Re-exports for macro hygiene (`$crate::chaos::assertions::_re_export::*`).
pub mod _re_export {
    pub use moonpool_explorer::{AssertCmp, AssertKind};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assertion_stats_new() {
        let stats = AssertionStats::new();
        assert_eq!(stats.total_checks, 0);
        assert_eq!(stats.successes, 0);
        assert_eq!(stats.success_rate(), 0.0);
    }

    #[test]
    fn test_assertion_stats_record() {
        let mut stats = AssertionStats::new();

        stats.record(true);
        assert_eq!(stats.total_checks, 1);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.success_rate(), 100.0);

        stats.record(false);
        assert_eq!(stats.total_checks, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.success_rate(), 50.0);

        stats.record(true);
        assert_eq!(stats.total_checks, 3);
        assert_eq!(stats.successes, 2);
        let expected = 200.0 / 3.0;
        assert!((stats.success_rate() - expected).abs() < 1e-10);
    }

    #[test]
    fn test_assertion_stats_success_rate_edge_cases() {
        let mut stats = AssertionStats::new();
        assert_eq!(stats.success_rate(), 0.0);

        stats.record(false);
        assert_eq!(stats.success_rate(), 0.0);

        stats.record(true);
        assert_eq!(stats.success_rate(), 50.0);
    }

    #[test]
    fn test_get_assertion_results_empty() {
        // When no assertions have been tracked, results should be empty
        // (assertion table not initialized = empty)
        let results = get_assertion_results();
        // May or may not be empty depending on prior test state,
        // but should not panic
        let _ = results;
    }

    #[test]
    fn test_validate_contracts_empty() {
        // Should produce no violations when no assertions tracked
        let violations = validate_assertion_contracts();
        // May or may not be empty, but should not panic
        let _ = violations;
    }
}
