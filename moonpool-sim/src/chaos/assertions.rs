//! Assertion macros and result tracking for simulation testing.
//!
//! This module provides `always_assert!` and `sometimes_assert!` macros for testing
//! distributed system properties. Assertions are tracked using thread-local storage
//! to enable statistical analysis of system behavior across multiple simulation runs.

use std::cell::RefCell;
use std::collections::HashMap;

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
    ///
    /// Calculate the success rate as a percentage (0.0 to 100.0).
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

thread_local! {
    /// Thread-local storage for assertion results.
    ///
    /// Each thread maintains independent assertion statistics, ensuring
    /// proper isolation between parallel test execution while allowing
    /// statistical collection within each simulation run.
    static ASSERTION_RESULTS: RefCell<HashMap<String, AssertionStats>> = RefCell::new(HashMap::new());
}

/// Record an assertion result for statistical tracking.
///
/// This function is used internally by the `sometimes_assert!` macro to track
/// assertion outcomes for later analysis.
///
/// # Parameters
///
/// * `name` - The assertion identifier
/// * `success` - Whether the assertion condition was true
pub fn record_assertion(name: &str, success: bool) {
    ASSERTION_RESULTS.with(|results| {
        let mut results = results.borrow_mut();
        let stats = results.entry(name.to_string()).or_default();
        stats.record(success);
    });
}

/// Get current assertion statistics for all tracked assertions.
///
/// Returns a snapshot of assertion results for the current thread.
/// This is typically called after simulation runs to analyze system behavior.
///
/// Get a snapshot of all assertion statistics collected so far.
pub fn get_assertion_results() -> HashMap<String, AssertionStats> {
    ASSERTION_RESULTS.with(|results| results.borrow().clone())
}

/// Reset all assertion statistics to empty state.
///
/// This should be called before each simulation run to ensure clean state
/// between consecutive simulations on the same thread.
///
/// Clear all assertion statistics.
pub fn reset_assertion_results() {
    ASSERTION_RESULTS.with(|results| {
        results.borrow_mut().clear();
    });
}

/// Check assertion validation and panic if violations are found.
///
/// This is a simplified function that checks for basic assertion issues.
/// It looks for assertions that never succeed (0% success rate).
///
/// # Parameters
///
/// * `_report` - The simulation report (unused in simplified version)
///
/// # Panics
///
/// Panics if there are assertions that never succeed.
pub fn panic_on_assertion_violations(_report: &crate::runner::SimulationReport) {
    let results = get_assertion_results();
    let mut violations = Vec::new();

    for (name, stats) in &results {
        if stats.total_checks > 0 && stats.success_rate() == 0.0 {
            violations.push(format!(
                "sometimes_assert!('{}') has 0% success rate (expected at least 1%)",
                name
            ));
        }
    }

    if !violations.is_empty() {
        println!("❌ Assertion violations found:");
        for violation in &violations {
            println!("  - {}", violation);
        }
        panic!("❌ Unexpected assertion violations detected!");
    } else {
        println!("✅ All assertions passed basic validation!");
    }
}

/// Validate that all `sometimes_assert!` assertions actually behave as "sometimes".
///
/// This simplified function checks that `sometimes_assert!` assertions have a
/// success rate of at least 1%.
///
/// # Returns
///
/// A vector of violation messages, or empty if all assertions are valid.
///
/// Validate that all assertion contracts have been met.
pub fn validate_assertion_contracts() -> Vec<String> {
    let mut violations = Vec::new();
    let results = get_assertion_results();

    for (name, stats) in &results {
        let rate = stats.success_rate();
        if stats.total_checks > 0 && rate == 0.0 {
            violations.push(format!(
                "sometimes_assert!('{}') has {:.1}% success rate (expected at least 1%)",
                name, rate
            ));
        }
    }

    violations
}

/// Notify the exploration framework that an `assert_sometimes!` succeeded.
///
/// This may trigger a fork if this is a new assertion discovery and
/// exploration is active.
pub fn on_sometimes_success(name: &str) {
    moonpool_explorer::maybe_fork_on_assertion(name);
}

/// Backing function for `assert_sometimes_each!`.
///
/// Delegates to the explorer's per-bucket fork infrastructure.
pub fn on_sometimes_each(msg: &str, keys: &[(&str, i64)], quality: &[(&str, i64)]) {
    moonpool_explorer::assertion_sometimes_each(msg, keys, quality);
}

/// Always-true assertion. Panics with seed info on failure.
///
/// Unlike the old `always_assert!`, this takes `(condition, message)` instead
/// of `(name, condition, message)`.
#[macro_export]
macro_rules! assert_always {
    ($condition:expr, $message:expr) => {
        if !$condition {
            let seed = $crate::sim::get_current_sim_seed();
            panic!("[ALWAYS FAILED] seed={} — {}", seed, $message);
        }
    };
}

/// Sometimes-true assertion. Records stats and triggers exploration fork on success.
///
/// Unlike the old `sometimes_assert!`, this takes `(condition, message)` instead
/// of `(name, condition, message)`. The message is used as the assertion name.
#[macro_export]
macro_rules! assert_sometimes {
    ($condition:expr, $message:expr) => {
        let result = $condition;
        $crate::chaos::assertions::record_assertion($message, result);
        if result {
            $crate::chaos::assertions::on_sometimes_success($message);
        }
    };
}

/// Per-value bucketed sometimes assertion. Forks on first discovery of each unique
/// key combination. Optional quality watermarks re-fork on improvement.
///
/// # Usage
///
/// ```ignore
/// // Identity keys only (one bucket per unique combination):
/// assert_sometimes_each!("msg", [("key1", val1), ("key2", val2)]);
///
/// // With quality watermarks (re-fork on improvement):
/// assert_sometimes_each!("msg", [("key1", val1)], [("quality", score)]);
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
    fn test_record_assertion_and_get_results() {
        reset_assertion_results();

        record_assertion("test1", true);
        record_assertion("test1", false);
        record_assertion("test2", true);

        let results = get_assertion_results();
        assert_eq!(results.len(), 2);

        let test1_stats = &results["test1"];
        assert_eq!(test1_stats.total_checks, 2);
        assert_eq!(test1_stats.successes, 1);
        assert_eq!(test1_stats.success_rate(), 50.0);

        let test2_stats = &results["test2"];
        assert_eq!(test2_stats.total_checks, 1);
        assert_eq!(test2_stats.successes, 1);
        assert_eq!(test2_stats.success_rate(), 100.0);
    }

    #[test]
    fn test_reset_assertion_results() {
        record_assertion("test", true);
        assert!(!get_assertion_results().is_empty());

        reset_assertion_results();
        assert!(get_assertion_results().is_empty());
    }

    #[test]
    fn test_assert_always_success() {
        reset_assertion_results();

        let value = 42;
        assert_always!(value == 42, "Value should be 42");

        // assert_always! does not track assertions - it only panics on failure
        let results = get_assertion_results();
        assert!(
            results.is_empty(),
            "assert_always! should not be tracked when successful"
        );
    }

    #[test]
    #[should_panic(expected = "[ALWAYS FAILED]")]
    fn test_assert_always_failure() {
        let value = 42;
        assert_always!(value == 0, "This should never happen");
    }

    #[test]
    fn test_assert_sometimes() {
        reset_assertion_results();

        let fast_time = 50;
        let slow_time = 150;
        let threshold = 100;

        assert_sometimes!(fast_time < threshold, "Operation should be fast");
        assert_sometimes!(slow_time < threshold, "Operation should be fast");

        let results = get_assertion_results();
        let stats = &results["Operation should be fast"];
        assert_eq!(stats.total_checks, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.success_rate(), 50.0);
    }

    #[test]
    fn test_assertion_isolation_between_tests() {
        // This test verifies that assertion results are isolated between tests
        reset_assertion_results();

        record_assertion("isolation_test", true);
        let results = get_assertion_results();
        assert_eq!(results["isolation_test"].total_checks, 1);

        // The isolation is ensured by thread-local storage and explicit resets
    }

    #[test]
    fn test_multiple_assertions_same_message() {
        reset_assertion_results();

        assert_sometimes!(true, "System should be reliable");
        assert_sometimes!(false, "System should be reliable");
        assert_sometimes!(true, "System should be reliable");
        assert_sometimes!(true, "System should be reliable");

        let results = get_assertion_results();
        let stats = &results["System should be reliable"];
        assert_eq!(stats.total_checks, 4);
        assert_eq!(stats.successes, 3);
        assert_eq!(stats.success_rate(), 75.0);
    }

    #[test]
    fn test_complex_assertion_conditions() {
        reset_assertion_results();

        let items = [1, 2, 3, 4, 5];
        let sum: i32 = items.iter().sum();

        assert_sometimes!(
            (10..=20).contains(&sum),
            "Sum should be in reasonable range"
        );

        assert_always!(!items.is_empty(), "Items list should not be empty");

        let results = get_assertion_results();
        // Only assert_sometimes! is tracked
        assert_eq!(results.len(), 1, "Only assert_sometimes should be tracked");
        assert_eq!(
            results["Sum should be in reasonable range"].success_rate(),
            100.0
        );
        // assert_always! does not appear in results when successful
        assert!(
            !results.contains_key("Items list should not be empty"),
            "assert_always should not be tracked"
        );
    }

    #[test]
    fn test_assert_sometimes_macro() {
        reset_assertion_results();

        // Test the new macro functionality
        assert_sometimes!(true, "Test assertion");
        assert_sometimes!(false, "Test assertion");

        let results = get_assertion_results();
        assert!(results.contains_key("Test assertion"));
        assert_eq!(results["Test assertion"].total_checks, 2);
        assert_eq!(results["Test assertion"].successes, 1);

        // Should not have violations (50% success rate is valid)
        let violations = validate_assertion_contracts();
        assert!(violations.is_empty());
    }
}
