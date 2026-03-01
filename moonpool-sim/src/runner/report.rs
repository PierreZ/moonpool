//! Simulation metrics and reporting.
//!
//! This module provides types for collecting and reporting simulation results.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use moonpool_explorer::AssertKind;

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

/// A captured bug recipe with its root seed for deterministic replay.
#[derive(Debug, Clone, PartialEq)]
pub struct BugRecipe {
    /// The root seed that was active when this bug was discovered.
    pub seed: u64,
    /// Fork path: `(rng_call_count, child_seed)` pairs.
    pub recipe: Vec<(u64, u64)>,
}

/// Report from fork-based exploration.
#[derive(Debug, Clone)]
pub struct ExplorationReport {
    /// Total timelines explored across all forks.
    pub total_timelines: u64,
    /// Total fork points triggered.
    pub fork_points: u64,
    /// Number of bugs found (children exiting with code 42).
    pub bugs_found: u64,
    /// Bug recipes captured during exploration (one per seed that found bugs).
    pub bug_recipes: Vec<BugRecipe>,
    /// Remaining global energy after exploration.
    pub energy_remaining: i64,
    /// Energy in the reallocation pool.
    pub realloc_pool_remaining: i64,
    /// Number of bits set in the explored coverage map.
    pub coverage_bits: u32,
    /// Total number of bits in the coverage map (8192).
    pub coverage_total: u32,
    /// Total instrumented code edges (from LLVM sancov). 0 when sancov unavailable.
    pub sancov_edges_total: usize,
    /// Code edges covered across all timelines. 0 when sancov unavailable.
    pub sancov_edges_covered: usize,
}

/// Pass/fail/miss status for an assertion in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AssertionStatus {
    /// Assertion contract violated (always-type failed, or unreachable reached).
    Fail,
    /// Coverage assertion never satisfied (sometimes/reachable never triggered).
    Miss,
    /// Assertion contract satisfied.
    Pass,
}

/// Detailed information about a single assertion slot.
#[derive(Debug, Clone)]
pub struct AssertionDetail {
    /// Human-readable assertion message.
    pub msg: String,
    /// The kind of assertion.
    pub kind: AssertKind,
    /// Number of times the assertion passed.
    pub pass_count: u64,
    /// Number of times the assertion failed.
    pub fail_count: u64,
    /// Best watermark value (for numeric assertions).
    pub watermark: i64,
    /// Frontier value (for BooleanSometimesAll).
    pub frontier: u8,
    /// Computed status based on kind and counts.
    pub status: AssertionStatus,
}

/// Summary of one `assert_sometimes_each!` site (grouped by msg).
#[derive(Debug, Clone)]
pub struct BucketSiteSummary {
    /// Assertion message identifying the site.
    pub msg: String,
    /// Number of unique identity-key buckets discovered.
    pub buckets_discovered: usize,
    /// Total hit count across all buckets.
    pub total_hits: u64,
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
    /// Always-type assertion violations (definite bugs).
    pub assertion_violations: Vec<String>,
    /// Coverage assertion violations (sometimes/reachable not satisfied).
    pub coverage_violations: Vec<String>,
    /// Exploration report (present when fork-based exploration was enabled).
    pub exploration: Option<ExplorationReport>,
    /// Per-assertion detailed breakdown from shared memory slots.
    pub assertion_details: Vec<AssertionDetail>,
    /// Per-site summaries of `assert_sometimes_each!` buckets.
    pub bucket_summaries: Vec<BucketSiteSummary>,
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

    /// Print the report to stderr with colors when the terminal supports it.
    ///
    /// Falls back to the plain `Display` output when stderr is not a TTY
    /// or `NO_COLOR` is set.
    pub fn eprint(&self) {
        super::display::eprint_report(self);
    }
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Format a number with comma separators (e.g., 123456 -> "123,456").
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Format an `i64` with comma separators (handles negatives).
fn fmt_i64(n: i64) -> String {
    if n < 0 {
        format!("-{}", fmt_num(n.unsigned_abs()))
    } else {
        fmt_num(n as u64)
    }
}

/// Format a duration as a human-readable string.
fn fmt_duration(d: Duration) -> String {
    let total_ms = d.as_millis();
    if total_ms < 1000 {
        format!("{}ms", total_ms)
    } else if total_ms < 60_000 {
        format!("{:.2}s", d.as_secs_f64())
    } else {
        let mins = d.as_secs() / 60;
        let secs = d.as_secs() % 60;
        format!("{}m {:02}s", mins, secs)
    }
}

/// Short human-readable label for an assertion kind.
fn kind_label(kind: AssertKind) -> &'static str {
    match kind {
        AssertKind::Always => "always",
        AssertKind::AlwaysOrUnreachable => "always?",
        AssertKind::Sometimes => "sometimes",
        AssertKind::Reachable => "reachable",
        AssertKind::Unreachable => "unreachable",
        AssertKind::NumericAlways => "num-always",
        AssertKind::NumericSometimes => "numeric",
        AssertKind::BooleanSometimesAll => "frontier",
    }
}

/// Sort key for grouping assertion kinds in display.
fn kind_sort_order(kind: AssertKind) -> u8 {
    match kind {
        AssertKind::Always => 0,
        AssertKind::AlwaysOrUnreachable => 1,
        AssertKind::Unreachable => 2,
        AssertKind::NumericAlways => 3,
        AssertKind::Sometimes => 4,
        AssertKind::Reachable => 5,
        AssertKind::NumericSometimes => 6,
        AssertKind::BooleanSometimesAll => 7,
    }
}

// ---------------------------------------------------------------------------
// Display impl
// ---------------------------------------------------------------------------

impl fmt::Display for SimulationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // === Header ===
        writeln!(f, "=== Simulation Report ===")?;
        writeln!(
            f,
            "  Iterations: {}  |  Passed: {}  |  Failed: {}  |  Rate: {:.1}%",
            self.iterations,
            self.successful_runs,
            self.failed_runs,
            self.success_rate()
        )?;
        writeln!(f)?;

        // === Timing ===
        writeln!(
            f,
            "  Avg Wall Time:     {:<14}Total: {}",
            fmt_duration(self.average_wall_time()),
            fmt_duration(self.metrics.wall_time)
        )?;
        writeln!(
            f,
            "  Avg Sim Time:      {}",
            fmt_duration(self.average_simulated_time())
        )?;
        writeln!(
            f,
            "  Avg Events:        {}",
            fmt_num(self.average_events_processed() as u64)
        )?;

        // === Faulty Seeds ===
        if !self.seeds_failing.is_empty() {
            writeln!(f)?;
            writeln!(f, "  Faulty seeds: {:?}", self.seeds_failing)?;
        }

        // === Exploration ===
        if let Some(ref exp) = self.exploration {
            writeln!(f)?;
            writeln!(f, "--- Exploration ---")?;
            writeln!(
                f,
                "  Timelines:    {:<18}Bugs found:     {}",
                fmt_num(exp.total_timelines),
                fmt_num(exp.bugs_found)
            )?;
            writeln!(
                f,
                "  Fork points:  {:<18}Coverage:       {} / {} bits ({:.1}%)",
                fmt_num(exp.fork_points),
                fmt_num(exp.coverage_bits as u64),
                fmt_num(exp.coverage_total as u64),
                if exp.coverage_total > 0 {
                    (exp.coverage_bits as f64 / exp.coverage_total as f64) * 100.0
                } else {
                    0.0
                }
            )?;
            if exp.sancov_edges_total > 0 {
                writeln!(
                    f,
                    "  Sancov:       {} / {} edges ({:.1}%)",
                    fmt_num(exp.sancov_edges_covered as u64),
                    fmt_num(exp.sancov_edges_total as u64),
                    (exp.sancov_edges_covered as f64 / exp.sancov_edges_total as f64) * 100.0
                )?;
            }
            writeln!(
                f,
                "  Energy left:  {:<18}Realloc pool:   {}",
                fmt_i64(exp.energy_remaining),
                fmt_i64(exp.realloc_pool_remaining)
            )?;
            for br in &exp.bug_recipes {
                writeln!(
                    f,
                    "  Bug recipe (seed={}): {}",
                    br.seed,
                    moonpool_explorer::format_timeline(&br.recipe)
                )?;
            }
        }

        // === Assertion Details ===
        if !self.assertion_details.is_empty() {
            writeln!(f)?;
            writeln!(f, "--- Assertions ({}) ---", self.assertion_details.len())?;

            let mut sorted: Vec<&AssertionDetail> = self.assertion_details.iter().collect();
            sorted.sort_by(|a, b| {
                kind_sort_order(a.kind)
                    .cmp(&kind_sort_order(b.kind))
                    .then(a.status.cmp(&b.status))
                    .then(a.msg.cmp(&b.msg))
            });

            for detail in &sorted {
                let status_tag = match detail.status {
                    AssertionStatus::Pass => "PASS",
                    AssertionStatus::Fail => "FAIL",
                    AssertionStatus::Miss => "MISS",
                };
                let kind_tag = kind_label(detail.kind);
                let quoted_msg = format!("\"{}\"", detail.msg);

                match detail.kind {
                    AssertKind::Sometimes | AssertKind::Reachable => {
                        let total = detail.pass_count + detail.fail_count;
                        let rate = if total > 0 {
                            (detail.pass_count as f64 / total as f64) * 100.0
                        } else {
                            0.0
                        };
                        writeln!(
                            f,
                            "  {}  [{:<10}]  {:<34}  {} / {} ({:.1}%)",
                            status_tag,
                            kind_tag,
                            quoted_msg,
                            fmt_num(detail.pass_count),
                            fmt_num(total),
                            rate
                        )?;
                    }
                    AssertKind::NumericSometimes | AssertKind::NumericAlways => {
                        writeln!(
                            f,
                            "  {}  [{:<10}]  {:<34}  {} pass  {} fail  watermark: {}",
                            status_tag,
                            kind_tag,
                            quoted_msg,
                            fmt_num(detail.pass_count),
                            fmt_num(detail.fail_count),
                            detail.watermark
                        )?;
                    }
                    AssertKind::BooleanSometimesAll => {
                        writeln!(
                            f,
                            "  {}  [{:<10}]  {:<34}  {} calls  frontier: {}",
                            status_tag,
                            kind_tag,
                            quoted_msg,
                            fmt_num(detail.pass_count),
                            detail.frontier
                        )?;
                    }
                    _ => {
                        // Always, AlwaysOrUnreachable, Unreachable
                        writeln!(
                            f,
                            "  {}  [{:<10}]  {:<34}  {} pass  {} fail",
                            status_tag,
                            kind_tag,
                            quoted_msg,
                            fmt_num(detail.pass_count),
                            fmt_num(detail.fail_count)
                        )?;
                    }
                }
            }
        }

        // === Assertion Violations ===
        if !self.assertion_violations.is_empty() {
            writeln!(f)?;
            writeln!(f, "--- Assertion Violations ---")?;
            for v in &self.assertion_violations {
                writeln!(f, "  - {}", v)?;
            }
        }

        // === Coverage Gaps ===
        if !self.coverage_violations.is_empty() {
            writeln!(f)?;
            writeln!(f, "--- Coverage Gaps ---")?;
            for v in &self.coverage_violations {
                writeln!(f, "  - {}", v)?;
            }
        }

        // === Buckets ===
        if !self.bucket_summaries.is_empty() {
            let total_buckets: usize = self
                .bucket_summaries
                .iter()
                .map(|s| s.buckets_discovered)
                .sum();
            writeln!(f)?;
            writeln!(
                f,
                "--- Buckets ({} across {} sites) ---",
                total_buckets,
                self.bucket_summaries.len()
            )?;
            for bs in &self.bucket_summaries {
                writeln!(
                    f,
                    "  {:<34}  {:>3} buckets  {:>8} hits",
                    format!("\"{}\"", bs.msg),
                    bs.buckets_discovered,
                    fmt_num(bs.total_hits)
                )?;
            }
        }

        // === Per-Seed Metrics ===
        if self.seeds_used.len() > 1 {
            writeln!(f)?;
            writeln!(f, "--- Seeds ---")?;
            for (i, seed) in self.seeds_used.iter().enumerate() {
                if let Some(Ok(m)) = self.individual_metrics.get(i) {
                    writeln!(
                        f,
                        "  #{:<3}  seed={:<14}  wall={:<10}  sim={:<10}  events={}",
                        i + 1,
                        seed,
                        fmt_duration(m.wall_time),
                        fmt_duration(m.simulated_time),
                        fmt_num(m.events_processed)
                    )?;
                } else if let Some(Err(_)) = self.individual_metrics.get(i) {
                    writeln!(f, "  #{:<3}  seed={:<14}  FAILED", i + 1, seed)?;
                }
            }
        }

        writeln!(f)?;
        Ok(())
    }
}
