//! Simulation metrics and reporting.
//!
//! This module provides types for collecting and reporting simulation results.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use crate::SimulationResult;
use crate::chaos::AssertionStats;

/// Core metrics collected during a simulation run.
#[derive(Debug, Clone, PartialEq)]
pub struct SimulationMetrics {
    /// Wall-clock time taken for the simulation
    pub wall_time: Duration,
    /// Simulated logical time elapsed
    pub simulated_time: Duration,
    /// Number of events processed
    pub events_processed: u64,
}

impl Default for SimulationMetrics {
    fn default() -> Self {
        Self {
            wall_time: Duration::ZERO,
            simulated_time: Duration::ZERO,
            events_processed: 0,
        }
    }
}

/// Comprehensive report of a simulation run with statistical analysis.
#[derive(Debug, Clone)]
pub struct SimulationReport {
    /// Number of iterations executed
    pub iterations: usize,
    /// Number of successful runs
    pub successful_runs: usize,
    /// Number of failed runs
    pub failed_runs: usize,
    /// Aggregated metrics across all runs
    pub metrics: SimulationMetrics,
    /// Individual metrics for each iteration
    pub individual_metrics: Vec<SimulationResult<SimulationMetrics>>,
    /// Seeds used for each iteration
    pub seeds_used: Vec<u64>,
    /// failed seeds
    pub seeds_failing: Vec<u64>,
    /// Aggregated assertion results across all iterations
    pub assertion_results: HashMap<String, AssertionStats>,
    /// Assertion validation violations (if any)
    pub assertion_violations: Vec<String>,
}

impl SimulationReport {
    /// Calculate the success rate as a percentage.
    pub fn success_rate(&self) -> f64 {
        if self.iterations == 0 {
            0.0
        } else {
            (self.successful_runs as f64 / self.iterations as f64) * 100.0
        }
    }

    /// Get the average wall time per iteration.
    pub fn average_wall_time(&self) -> Duration {
        if self.successful_runs == 0 {
            Duration::ZERO
        } else {
            self.metrics.wall_time / self.successful_runs as u32
        }
    }

    /// Get the average simulated time per iteration.
    pub fn average_simulated_time(&self) -> Duration {
        if self.successful_runs == 0 {
            Duration::ZERO
        } else {
            self.metrics.simulated_time / self.successful_runs as u32
        }
    }

    /// Get the average number of events processed per iteration.
    pub fn average_events_processed(&self) -> f64 {
        if self.successful_runs == 0 {
            0.0
        } else {
            self.metrics.events_processed as f64 / self.successful_runs as f64
        }
    }
}

impl fmt::Display for SimulationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Simulation Report ===")?;
        writeln!(f, "Iterations: {}", self.iterations)?;
        writeln!(f, "Successful: {}", self.successful_runs)?;
        writeln!(f, "Failed: {}", self.failed_runs)?;
        writeln!(f, "Success Rate: {:.2}%", self.success_rate())?;
        writeln!(f)?;
        writeln!(f, "Average Wall Time: {:?}", self.average_wall_time())?;
        writeln!(
            f,
            "Average Simulated Time: {:?}",
            self.average_simulated_time()
        )?;
        writeln!(
            f,
            "Average Events Processed: {:.1}",
            self.average_events_processed()
        )?;

        if !self.seeds_failing.is_empty() {
            writeln!(f)?;
            writeln!(f, "Faulty seeds: {:?}", self.seeds_failing)?;
        }

        writeln!(f)?;

        Ok(())
    }
}
