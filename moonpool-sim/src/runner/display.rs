//! Colored terminal display for simulation reports.
//!
//! Provides rich, colorized output for TTY stderr. Falls back to the plain
//! `Display` impl when stderr is not a terminal or `NO_COLOR` is set.

use std::io::{IsTerminal, Write};

use moonpool_explorer::AssertKind;

use super::report::{
    AssertionDetail, AssertionStatus, BucketSiteSummary, ExplorationReport, SimulationReport,
};

// ---------------------------------------------------------------------------
// ANSI escape helpers
// ---------------------------------------------------------------------------

mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const BOLD_RED: &str = "\x1b[1;31m";
    pub const BOLD_GREEN: &str = "\x1b[1;32m";
    pub const BOLD_YELLOW: &str = "\x1b[1;33m";
    pub const BOLD_CYAN: &str = "\x1b[1;36m";
}

/// Whether to emit ANSI color codes.
fn use_color() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

// ---------------------------------------------------------------------------
// Formatting helpers (reused from report.rs, extended)
// ---------------------------------------------------------------------------

/// Format a `u64` with comma separators.
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

/// Format an `i64` with comma separators.
fn fmt_i64(n: i64) -> String {
    if n < 0 {
        format!("-{}", fmt_num(n.unsigned_abs()))
    } else {
        fmt_num(n as u64)
    }
}

/// Format a duration as a human-readable string.
fn fmt_duration(d: std::time::Duration) -> String {
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

/// Short label for an assertion kind.
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

/// Sort key for grouping assertion kinds.
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
// Progress bar
// ---------------------------------------------------------------------------

const BAR_WIDTH: usize = 20;

/// Render a progress bar: `████████░░░░░░░░░░░░  62.5%`
fn progress_bar(fraction: f64, color: bool) -> String {
    let filled = ((fraction * BAR_WIDTH as f64).round() as usize).min(BAR_WIDTH);
    let empty = BAR_WIDTH - filled;

    let bar_color = if !color {
        ""
    } else if fraction >= 0.5 {
        ansi::GREEN
    } else if fraction >= 0.2 {
        ansi::YELLOW
    } else {
        ansi::RED
    };
    let reset = if color { ansi::RESET } else { "" };

    format!(
        "{}{}{}{}  {:.1}%",
        bar_color,
        "█".repeat(filled),
        "░".repeat(empty),
        reset,
        fraction * 100.0
    )
}

// ---------------------------------------------------------------------------
// Section header
// ---------------------------------------------------------------------------

const RULE_WIDTH: usize = 56;

/// Print a section header like: `━━━ Title ━━━━━━━━━━━━━━━━━━━━━━`
fn section_header(w: &mut impl Write, title: &str, color: bool, style: &str) {
    let prefix = "━━━ ";
    let suffix_char = '━';
    // title + spaces around it
    let content_len = prefix.len() + title.len() + 1; // +1 for trailing space
    let trail = if RULE_WIDTH > content_len {
        RULE_WIDTH - content_len
    } else {
        3
    };

    if color {
        let _ = write!(w, "\n{style}{prefix}{title} ", style = style);
        for _ in 0..trail {
            let _ = write!(w, "{suffix_char}");
        }
        let _ = writeln!(w, "{}", ansi::RESET);
    } else {
        let _ = write!(w, "\n{prefix}{title} ");
        for _ in 0..trail {
            let _ = write!(w, "{suffix_char}");
        }
        let _ = writeln!(w);
    }
}

// ---------------------------------------------------------------------------
// Status indicators
// ---------------------------------------------------------------------------

fn status_icon(status: AssertionStatus, color: bool) -> &'static str {
    match (status, color) {
        (AssertionStatus::Pass, true) => "\x1b[32m✓\x1b[0m",
        (AssertionStatus::Fail, true) => "\x1b[1;31m✗\x1b[0m",
        (AssertionStatus::Miss, true) => "\x1b[33m○\x1b[0m",
        (AssertionStatus::Pass, false) => "✓",
        (AssertionStatus::Fail, false) => "✗",
        (AssertionStatus::Miss, false) => "○",
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Print the simulation report to stderr with colors if supported.
///
/// Falls back to the plain `Display` impl when stderr is not a terminal
/// or the `NO_COLOR` environment variable is set.
pub fn eprint_report(report: &SimulationReport) {
    let color = use_color();
    let mut w = std::io::stderr().lock();
    write_report(&mut w, report, color);
}

fn write_report(w: &mut impl Write, report: &SimulationReport, color: bool) {
    // === Header ===
    section_header(w, "Simulation Report", color, ansi::BOLD_CYAN);

    // Summary line with colored pass/fail indicator
    let rate = report.success_rate();
    let (rate_icon, rate_color) = if report.failed_runs == 0 {
        ("✓", ansi::BOLD_GREEN)
    } else {
        ("✗", ansi::BOLD_RED)
    };

    if color {
        let _ = writeln!(
            w,
            "  {} iterations   {} passed   {} failed   {rate_color}{rate_icon} {rate:.1}%{reset}",
            report.iterations,
            report.successful_runs,
            report.failed_runs,
            rate_color = rate_color,
            rate_icon = rate_icon,
            rate = rate,
            reset = ansi::RESET,
        );
    } else {
        let _ = writeln!(
            w,
            "  {} iterations   {} passed   {} failed   {rate_icon} {rate:.1}%",
            report.iterations,
            report.successful_runs,
            report.failed_runs,
            rate_icon = rate_icon,
            rate = rate,
        );
    }

    // Timing
    let _ = writeln!(w);
    let _ = writeln!(
        w,
        "  Wall Time    {:<14} {} total",
        fmt_duration(report.average_wall_time()) + " avg",
        fmt_duration(report.metrics.wall_time),
    );
    let _ = writeln!(
        w,
        "  Sim Time     {} avg",
        fmt_duration(report.average_simulated_time()),
    );
    let _ = writeln!(
        w,
        "  Events       {} avg",
        fmt_num(report.average_events_processed() as u64),
    );

    // Faulty seeds
    if !report.seeds_failing.is_empty() {
        let _ = writeln!(w);
        if color {
            let _ = write!(w, "  {}Faulty seeds:{} ", ansi::BOLD_RED, ansi::RESET);
        } else {
            let _ = write!(w, "  Faulty seeds: ");
        }
        let _ = writeln!(w, "{:?}", report.seeds_failing);
    }

    // === Exploration ===
    if let Some(ref exp) = report.exploration {
        write_exploration(w, exp, color);
    }

    // === Assertions ===
    if !report.assertion_details.is_empty() {
        write_assertions(w, &report.assertion_details, color);
    }

    // === Violations ===
    if !report.assertion_violations.is_empty() {
        section_header(w, "Violations", color, ansi::BOLD_RED);
        for v in &report.assertion_violations {
            if color {
                let _ = writeln!(w, "  {}✗{}  {}", ansi::BOLD_RED, ansi::RESET, v);
            } else {
                let _ = writeln!(w, "  ✗  {v}");
            }
        }
    }

    // === Coverage Gaps ===
    if !report.coverage_violations.is_empty() {
        section_header(w, "Coverage Gaps", color, ansi::BOLD_YELLOW);
        for v in &report.coverage_violations {
            if color {
                let _ = writeln!(w, "  {}○{}  {}", ansi::YELLOW, ansi::RESET, v);
            } else {
                let _ = writeln!(w, "  ○  {v}");
            }
        }
    }

    // === Buckets ===
    if !report.bucket_summaries.is_empty() {
        write_buckets(w, &report.bucket_summaries, color);
    }

    // === Per-Seed Metrics ===
    if report.seeds_used.len() > 1 {
        write_seeds(w, report, color);
    }

    let _ = writeln!(w);
}

// ---------------------------------------------------------------------------
// Sub-sections
// ---------------------------------------------------------------------------

fn write_exploration(w: &mut impl Write, exp: &ExplorationReport, color: bool) {
    section_header(w, "Exploration", color, ansi::BOLD_CYAN);

    let _ = writeln!(
        w,
        "  Timelines    {:<16} Bugs         {}",
        fmt_num(exp.total_timelines),
        fmt_num(exp.bugs_found),
    );
    let _ = writeln!(
        w,
        "  Fork Points  {:<16} Energy       {} remaining",
        fmt_num(exp.fork_points),
        fmt_i64(exp.energy_remaining),
    );

    if exp.realloc_pool_remaining != 0 {
        let _ = writeln!(w, "  Realloc Pool {}", fmt_i64(exp.realloc_pool_remaining),);
    }

    // Progress bars
    let _ = writeln!(w);
    if exp.coverage_total > 0 {
        let frac = exp.coverage_bits as f64 / exp.coverage_total as f64;
        let _ = writeln!(
            w,
            "  Exploration  {}   {} / {} bits",
            progress_bar(frac, color),
            fmt_num(exp.coverage_bits as u64),
            fmt_num(exp.coverage_total as u64),
        );
    }
    if exp.sancov_edges_total > 0 {
        let frac = exp.sancov_edges_covered as f64 / exp.sancov_edges_total as f64;
        let _ = writeln!(
            w,
            "  Code Cov     {}   {} / {} edges",
            progress_bar(frac, color),
            fmt_num(exp.sancov_edges_covered as u64),
            fmt_num(exp.sancov_edges_total as u64),
        );
    }

    // Bug recipes
    if !exp.bug_recipes.is_empty() {
        let _ = writeln!(w);
        if color {
            let _ = writeln!(w, "  {}Bug Recipes{}", ansi::BOLD, ansi::RESET);
        } else {
            let _ = writeln!(w, "  Bug Recipes");
        }
        for br in &exp.bug_recipes {
            let _ = writeln!(
                w,
                "    seed={}: {}",
                br.seed,
                moonpool_explorer::format_timeline(&br.recipe),
            );
        }
    }
}

fn write_assertions(w: &mut impl Write, details: &[AssertionDetail], color: bool) {
    section_header(
        w,
        &format!("Assertions ({})", details.len()),
        color,
        ansi::BOLD_CYAN,
    );

    let mut sorted: Vec<&AssertionDetail> = details.iter().collect();
    sorted.sort_by(|a, b| {
        kind_sort_order(a.kind)
            .cmp(&kind_sort_order(b.kind))
            .then(a.status.cmp(&b.status))
            .then(a.msg.cmp(&b.msg))
    });

    // Compute max message length for alignment (capped)
    let max_msg = sorted
        .iter()
        .map(|d| d.msg.len())
        .max()
        .unwrap_or(0)
        .min(40);

    for detail in &sorted {
        let icon = status_icon(detail.status, color);
        let kind = kind_label(detail.kind);
        let msg = &detail.msg;
        // Truncate very long messages
        let display_msg = if msg.len() > 40 {
            format!("\"{}...\"", &msg[..37])
        } else {
            format!("\"{msg}\"")
        };

        let stats = match detail.kind {
            AssertKind::Sometimes | AssertKind::Reachable => {
                let total = detail.pass_count + detail.fail_count;
                let rate = if total > 0 {
                    (detail.pass_count as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                format!(
                    "{} / {}  ({:.1}%)",
                    fmt_num(detail.pass_count),
                    fmt_num(total),
                    rate
                )
            }
            AssertKind::NumericSometimes | AssertKind::NumericAlways => {
                format!(
                    "{} pass  {} fail  best: {}",
                    fmt_num(detail.pass_count),
                    fmt_num(detail.fail_count),
                    detail.watermark,
                )
            }
            AssertKind::BooleanSometimesAll => {
                format!(
                    "{} calls  frontier: {}",
                    fmt_num(detail.pass_count),
                    detail.frontier,
                )
            }
            _ => {
                // Always, AlwaysOrUnreachable, Unreachable
                format!(
                    "{} pass  {} fail",
                    fmt_num(detail.pass_count),
                    fmt_num(detail.fail_count),
                )
            }
        };

        // Pad message for alignment
        let pad = if max_msg + 2 > display_msg.len() {
            max_msg + 2 - display_msg.len()
        } else {
            1
        };

        let _ = writeln!(
            w,
            "  {icon}  {kind:<12} {display_msg}{padding}{stats}",
            icon = icon,
            kind = kind,
            display_msg = display_msg,
            padding = " ".repeat(pad),
            stats = stats,
        );
    }
}

fn write_buckets(w: &mut impl Write, summaries: &[BucketSiteSummary], color: bool) {
    let total_buckets: usize = summaries.iter().map(|s| s.buckets_discovered).sum();
    section_header(
        w,
        &format!(
            "Buckets ({} across {} sites)",
            total_buckets,
            summaries.len()
        ),
        color,
        ansi::BOLD_CYAN,
    );
    for bs in summaries {
        let _ = writeln!(
            w,
            "  {:<34}  {:>3} buckets  {:>8} hits",
            format!("\"{}\"", bs.msg),
            bs.buckets_discovered,
            fmt_num(bs.total_hits),
        );
    }
}

fn write_seeds(w: &mut impl Write, report: &SimulationReport, color: bool) {
    section_header(w, "Seeds", color, ansi::BOLD_CYAN);

    let dim = if color { ansi::DIM } else { "" };
    let reset = if color { ansi::RESET } else { "" };

    for (i, seed) in report.seeds_used.iter().enumerate() {
        if let Some(Ok(m)) = report.individual_metrics.get(i) {
            let is_failed = report.seeds_failing.contains(seed);
            if is_failed && color {
                let _ = writeln!(
                    w,
                    "  {red}#{:<3}  seed={:<14}  {}   {} sim   {} events{reset}",
                    i + 1,
                    seed,
                    fmt_duration(m.wall_time),
                    fmt_duration(m.simulated_time),
                    fmt_num(m.events_processed),
                    red = ansi::RED,
                    reset = ansi::RESET,
                );
            } else {
                let _ = writeln!(
                    w,
                    "  {dim}#{:<3}  seed={:<14}  {}   {} sim   {} events{reset}",
                    i + 1,
                    seed,
                    fmt_duration(m.wall_time),
                    fmt_duration(m.simulated_time),
                    fmt_num(m.events_processed),
                    dim = dim,
                    reset = reset,
                );
            }
        } else if let Some(Err(_)) = report.individual_metrics.get(i) {
            if color {
                let _ = writeln!(
                    w,
                    "  {red}#{:<3}  seed={:<14}  FAILED{reset}",
                    i + 1,
                    seed,
                    red = ansi::BOLD_RED,
                    reset = ansi::RESET,
                );
            } else {
                let _ = writeln!(w, "  #{:<3}  seed={:<14}  FAILED", i + 1, seed);
            }
        }
    }
}
