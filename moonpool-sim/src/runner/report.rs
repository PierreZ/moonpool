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
    /// Whether the multi-seed loop stopped because convergence was detected.
    pub converged: bool,
    /// Timelines explored per seed (parallel to `seeds_used`).
    pub per_seed_timelines: Vec<u64>,
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
    /// Frontier value (for `BooleanSometimesAll`).
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
    /// True when `UntilConverged` hit its iteration cap without converging.
    pub convergence_timeout: bool,
}

impl SimulationReport {
    /// Calculate the success rate as a percentage.
    #[must_use]
    pub fn success_rate(&self) -> f64 {
        if self.iterations == 0 {
            0.0
        } else {
            // Precision loss acceptable: simulation counts fit well within 2^52.
            let successful = u32::try_from(self.successful_runs).map_or(f64::INFINITY, f64::from);
            let total = u32::try_from(self.iterations).map_or(f64::INFINITY, f64::from);
            (successful / total) * 100.0
        }
    }

    /// Get the average wall time per iteration.
    #[must_use]
    pub fn average_wall_time(&self) -> Duration {
        if self.successful_runs == 0 {
            Duration::ZERO
        } else {
            let runs = u32::try_from(self.successful_runs).unwrap_or(u32::MAX);
            self.metrics.wall_time / runs
        }
    }

    /// Get the average simulated time per iteration.
    #[must_use]
    pub fn average_simulated_time(&self) -> Duration {
        if self.successful_runs == 0 {
            Duration::ZERO
        } else {
            let runs = u32::try_from(self.successful_runs).unwrap_or(u32::MAX);
            self.metrics.simulated_time / runs
        }
    }

    /// Get the average number of events processed per iteration.
    #[must_use]
    pub fn average_events_processed(&self) -> f64 {
        if self.successful_runs == 0 {
            0.0
        } else {
            // Precision loss acceptable: simulation counts fit within 2^52.
            let events =
                u32::try_from(self.metrics.events_processed).map_or(f64::INFINITY, f64::from);
            let runs = u32::try_from(self.successful_runs).map_or(f64::INFINITY, f64::from);
            events / runs
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

/// Convert a non-negative finite `f64` to a saturated `u64`.
///
/// Returns 0 for NaN / negative values and `u64::MAX` for values that exceed
/// `u64::MAX`. Smaller magnitudes round to the nearest integer.
fn f64_to_u64_saturating(v: f64) -> u64 {
    // `2^64` as an `f64` — values at or above this saturate to `u64::MAX`.
    const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
    if !v.is_finite() || v <= 0.0 {
        0
    } else if v >= TWO_POW_64 {
        u64::MAX
    } else {
        // SAFETY: `v` is finite, non-negative, and strictly below `2^64`;
        // `to_int_unchecked` is therefore well-defined.
        unsafe { v.round().to_int_unchecked::<u64>() }
    }
}

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
        fmt_num(n.unsigned_abs())
    }
}

/// Format a duration as a human-readable string.
fn fmt_duration(d: Duration) -> String {
    let total_ms = d.as_millis();
    if total_ms < 1000 {
        format!("{total_ms}ms")
    } else if total_ms < 60_000 {
        format!("{:.2}s", d.as_secs_f64())
    } else {
        let mins = d.as_secs() / 60;
        let secs = d.as_secs() % 60;
        format!("{mins}m {secs:02}s")
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

fn fmt_summary(f: &mut fmt::Formatter<'_>, report: &SimulationReport) -> fmt::Result {
    writeln!(f, "=== Simulation Report ===")?;
    writeln!(
        f,
        "  Iterations: {}  |  Passed: {}  |  Failed: {}  |  Rate: {:.1}%",
        report.iterations,
        report.successful_runs,
        report.failed_runs,
        report.success_rate()
    )?;
    writeln!(f)
}

fn fmt_timing(f: &mut fmt::Formatter<'_>, report: &SimulationReport) -> fmt::Result {
    writeln!(
        f,
        "  Avg Wall Time:     {:<14}Total: {}",
        fmt_duration(report.average_wall_time()),
        fmt_duration(report.metrics.wall_time)
    )?;
    writeln!(
        f,
        "  Avg Sim Time:      {}",
        fmt_duration(report.average_simulated_time())
    )?;
    writeln!(
        f,
        "  Avg Events:        {}",
        fmt_num(f64_to_u64_saturating(report.average_events_processed()))
    )
}

fn fmt_exploration(f: &mut fmt::Formatter<'_>, exp: &ExplorationReport) -> fmt::Result {
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
        fmt_num(u64::from(exp.coverage_bits)),
        fmt_num(u64::from(exp.coverage_total)),
        if exp.coverage_total > 0 {
            (f64::from(exp.coverage_bits) / f64::from(exp.coverage_total)) * 100.0
        } else {
            0.0
        }
    )?;
    if exp.sancov_edges_total > 0 {
        let covered = u64::try_from(exp.sancov_edges_covered).unwrap_or(u64::MAX);
        let total = u64::try_from(exp.sancov_edges_total).unwrap_or(u64::MAX);
        let covered_u32 = u32::try_from(exp.sancov_edges_covered).unwrap_or(u32::MAX);
        let total_u32 = u32::try_from(exp.sancov_edges_total).unwrap_or(u32::MAX);
        writeln!(
            f,
            "  Sancov:       {} / {} edges ({:.1}%)",
            fmt_num(covered),
            fmt_num(total),
            (f64::from(covered_u32) / f64::from(total_u32)) * 100.0
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
    Ok(())
}

fn fmt_assertion_detail(f: &mut fmt::Formatter<'_>, detail: &AssertionDetail) -> fmt::Result {
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
                let pass_u32 = u32::try_from(detail.pass_count).unwrap_or(u32::MAX);
                let total_u32 = u32::try_from(total).unwrap_or(u32::MAX);
                (f64::from(pass_u32) / f64::from(total_u32)) * 100.0
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
            )
        }
        AssertKind::NumericSometimes | AssertKind::NumericAlways => writeln!(
            f,
            "  {}  [{:<10}]  {:<34}  {} pass  {} fail  watermark: {}",
            status_tag,
            kind_tag,
            quoted_msg,
            fmt_num(detail.pass_count),
            fmt_num(detail.fail_count),
            detail.watermark
        ),
        AssertKind::BooleanSometimesAll => writeln!(
            f,
            "  {}  [{:<10}]  {:<34}  {} calls  frontier: {}",
            status_tag,
            kind_tag,
            quoted_msg,
            fmt_num(detail.pass_count),
            detail.frontier
        ),
        _ => writeln!(
            f,
            "  {}  [{:<10}]  {:<34}  {} pass  {} fail",
            status_tag,
            kind_tag,
            quoted_msg,
            fmt_num(detail.pass_count),
            fmt_num(detail.fail_count)
        ),
    }
}

fn fmt_assertion_details(f: &mut fmt::Formatter<'_>, details: &[AssertionDetail]) -> fmt::Result {
    if details.is_empty() {
        return Ok(());
    }
    writeln!(f)?;
    writeln!(f, "--- Assertions ({}) ---", details.len())?;

    let mut sorted: Vec<&AssertionDetail> = details.iter().collect();
    sorted.sort_by(|a, b| {
        kind_sort_order(a.kind)
            .cmp(&kind_sort_order(b.kind))
            .then(a.status.cmp(&b.status))
            .then(a.msg.cmp(&b.msg))
    });

    for detail in &sorted {
        fmt_assertion_detail(f, detail)?;
    }
    Ok(())
}

impl fmt::Display for SimulationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_summary(f, self)?;
        fmt_timing(f, self)?;

        // === Faulty Seeds ===
        if !self.seeds_failing.is_empty() {
            writeln!(f)?;
            writeln!(f, "  Faulty seeds: {:?}", self.seeds_failing)?;
        }

        if let Some(ref exp) = self.exploration {
            fmt_exploration(f, exp)?;
        }

        fmt_assertion_details(f, &self.assertion_details)?;

        // === Assertion Violations ===
        if !self.assertion_violations.is_empty() {
            writeln!(f)?;
            writeln!(f, "--- Assertion Violations ---")?;
            for v in &self.assertion_violations {
                writeln!(f, "  - {v}")?;
            }
        }

        // === Coverage Gaps ===
        if !self.coverage_violations.is_empty() {
            writeln!(f)?;
            writeln!(f, "--- Coverage Gaps ---")?;
            for v in &self.coverage_violations {
                writeln!(f, "  - {v}")?;
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

        // === Convergence Timeout ===
        if self.convergence_timeout {
            writeln!(f)?;
            writeln!(f, "--- Convergence FAILED ---")?;
            writeln!(f, "  UntilConverged hit iteration cap without converging.")?;
        }

        // === Per-Seed Metrics ===
        if self.seeds_used.len() > 1 {
            writeln!(f)?;
            writeln!(f, "--- Seeds ---")?;
            let per_seed_tl = self.exploration.as_ref().map(|e| &e.per_seed_timelines);
            for (i, seed) in self.seeds_used.iter().enumerate() {
                if let Some(Ok(m)) = self.individual_metrics.get(i) {
                    let tl_suffix = per_seed_tl
                        .and_then(|v| v.get(i))
                        .map(|t| format!("  timelines={}", fmt_num(*t)))
                        .unwrap_or_default();
                    writeln!(
                        f,
                        "  #{:<3}  seed={:<14}  wall={:<10}  sim={:<10}  events={}{}",
                        i + 1,
                        seed,
                        fmt_duration(m.wall_time),
                        fmt_duration(m.simulated_time),
                        fmt_num(m.events_processed),
                        tl_suffix,
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
