//! Shared formatting helpers for runner reports and terminal display.

use std::time::Duration;

/// Format a `u64` with comma separators (e.g., 123456 -> "123,456").
pub(crate) fn fmt_num(n: u64) -> String {
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
pub(crate) fn fmt_i64(n: i64) -> String {
    if n < 0 {
        format!("-{}", fmt_num(n.unsigned_abs()))
    } else {
        fmt_num(n as u64)
    }
}

/// Format a duration as a human-readable string.
pub(crate) fn fmt_duration(d: Duration) -> String {
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
