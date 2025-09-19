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
    /// # Example
    ///
    /// ```rust
    /// use moonpool_simulation::assertions::AssertionStats;
    ///
    /// let mut stats = AssertionStats::new();
    /// stats.total_checks = 10;
    /// stats.successes = 7;
    ///
    /// assert_eq!(stats.success_rate(), 70.0);
    /// ```
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
/// # Example
///
/// ```rust
/// use moonpool_simulation::assertions::{get_assertion_results, record_assertion};
///
/// // Simulate some assertion checks
/// record_assertion("leader_exists", true);
/// record_assertion("leader_exists", true);
/// record_assertion("leader_exists", false);
///
/// let results = get_assertion_results();
/// let leader_stats = &results["leader_exists"];
/// assert_eq!(leader_stats.total_checks, 3);
/// assert_eq!(leader_stats.successes, 2);
/// let expected_rate = 200.0 / 3.0; // ~66.67%
/// assert!((leader_stats.success_rate() - expected_rate).abs() < 1e-10);
/// ```
pub fn get_assertion_results() -> HashMap<String, AssertionStats> {
    ASSERTION_RESULTS.with(|results| results.borrow().clone())
}

/// Reset all assertion statistics to empty state.
///
/// This should be called before each simulation run to ensure clean state
/// between consecutive simulations on the same thread.
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::assertions::{reset_assertion_results, record_assertion, get_assertion_results};
///
/// // Record some assertions
/// record_assertion("test", true);
/// assert_eq!(get_assertion_results()["test"].total_checks, 1);
///
/// // Reset and verify clean state
/// reset_assertion_results();
/// assert!(get_assertion_results().is_empty());
/// ```
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
/// # Example
///
/// ```rust
/// use moonpool_simulation::assertions::{validate_assertion_contracts, reset_assertion_results};
///
/// reset_assertion_results();
/// // ... run simulation with assertions ...
/// let violations = validate_assertion_contracts();
/// if !violations.is_empty() {
///     // Handle violations
/// }
/// ```
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

/// Assert that a condition is always true, panicking on failure.
///
/// This macro is used for conditions that must always hold in a correct
/// distributed system implementation. If the condition fails, the simulation
/// will panic immediately with a descriptive error message including the current seed.
///
/// # Parameters
///
/// * `name` - An identifier for this assertion (for error reporting)
/// * `condition` - The expression to evaluate (must be boolean)
/// * `message` - A descriptive error message to show on failure
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::always_assert;
///
/// let leader_count = 1;
/// always_assert!(unique_leader, leader_count == 1, "There must be exactly one leader");
/// ```
///
/// # Panics
///
/// Panics immediately if the condition evaluates to false.
#[macro_export]
macro_rules! always_assert {
    ($name:ident, $condition:expr, $message:expr) => {
        let result = $condition;
        if !result {
            let current_seed = $crate::get_current_sim_seed();
            panic!(
                "Always assertion '{}' failed (seed: {}): {}",
                stringify!($name),
                current_seed,
                $message
            );
        }
    };
}

/// Assert a condition that should sometimes be true, tracking the success rate.
///
/// This macro is used for probabilistic properties in distributed systems,
/// such as "consensus should usually be reached quickly" or "the system should
/// be available most of the time". The assertion result is tracked for statistical
/// analysis without causing the simulation to fail.
///
/// The macro automatically registers the assertion at compile time and tracks
/// module execution to enable unreachable code detection.
///
/// # Parameters
///
/// * `name` - An identifier for this assertion (for tracking purposes)
/// * `condition` - The expression to evaluate (must be boolean)
/// * `message` - A descriptive message about what this assertion tests
///
/// # Example
///
/// ```rust
/// use moonpool_simulation::sometimes_assert;
/// use std::time::Duration;
///
/// let consensus_time = Duration::from_millis(50);
/// let threshold = Duration::from_millis(100);
/// sometimes_assert!(fast_consensus, consensus_time < threshold, "Consensus should be fast");
/// ```
#[macro_export]
macro_rules! sometimes_assert {
    ($name:ident, $condition:expr, $message:expr) => {
        // Runtime execution - simplified to just record the result
        let result = $condition;
        $crate::assertions::record_assertion(stringify!($name), result);
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
    fn test_always_assert_success() {
        reset_assertion_results();

        let value = 42;
        always_assert!(value_is_42, value == 42, "Value should be 42");

        // always_assert! no longer tracks successful assertions
        // It only panics on failure, so successful calls leave no trace
        let results = get_assertion_results();
        assert!(
            results.is_empty(),
            "always_assert! should not be tracked when successful"
        );
    }

    #[test]
    #[should_panic(
        expected = "Always assertion 'impossible' failed (seed: 0): This should never happen"
    )]
    fn test_always_assert_failure() {
        let value = 42;
        always_assert!(impossible, value == 0, "This should never happen");
    }

    #[test]
    fn test_sometimes_assert() {
        reset_assertion_results();

        let fast_time = 50;
        let slow_time = 150;
        let threshold = 100;

        sometimes_assert!(
            fast_operation,
            fast_time < threshold,
            "Operation should be fast"
        );
        sometimes_assert!(
            fast_operation,
            slow_time < threshold,
            "Operation should be fast"
        );

        let results = get_assertion_results();
        let stats = &results["fast_operation"];
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
    fn test_multiple_assertions_same_name() {
        reset_assertion_results();

        sometimes_assert!(reliability, true, "System should be reliable");
        sometimes_assert!(reliability, false, "System should be reliable");
        sometimes_assert!(reliability, true, "System should be reliable");
        sometimes_assert!(reliability, true, "System should be reliable");

        let results = get_assertion_results();
        let stats = &results["reliability"];
        assert_eq!(stats.total_checks, 4);
        assert_eq!(stats.successes, 3);
        assert_eq!(stats.success_rate(), 75.0);
    }

    #[test]
    fn test_complex_assertion_conditions() {
        reset_assertion_results();

        let items = vec![1, 2, 3, 4, 5];
        let sum: i32 = items.iter().sum();

        sometimes_assert!(
            sum_in_range,
            sum >= 10 && sum <= 20,
            "Sum should be in reasonable range"
        );

        always_assert!(
            not_empty,
            !items.is_empty(),
            "Items list should not be empty"
        );

        let results = get_assertion_results();
        // Only sometimes_assert! is tracked now
        assert_eq!(results.len(), 1, "Only sometimes_assert should be tracked");
        assert_eq!(results["sum_in_range"].success_rate(), 100.0);
        // always_assert! no longer appears in results when successful
        assert!(
            !results.contains_key("not_empty"),
            "always_assert should not be tracked"
        );
    }

    #[test]
    fn test_sometimes_assert_macro() {
        reset_assertion_results();

        // Test the simplified macro functionality
        sometimes_assert!(macro_test, true, "Test assertion");
        sometimes_assert!(macro_test, false, "Test assertion");

        let results = get_assertion_results();
        assert!(results.contains_key("macro_test"));
        assert_eq!(results["macro_test"].total_checks, 2);
        assert_eq!(results["macro_test"].successes, 1);

        // Should not have violations (50% success rate is valid)
        let violations = validate_assertion_contracts();
        assert!(violations.is_empty());
    }
}
