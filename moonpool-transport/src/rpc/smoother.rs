//! Exponential moving average (EMA) smoother for latency / queue-depth tracking.
//!
//! Direct port of FoundationDB's `fdbrpc/include/fdbrpc/Smoother.h`. Used by
//! [`crate::rpc::QueueModel`] to track smoothed in-flight request counts and
//! latency per endpoint.
//!
//! # Semantics
//!
//! `Smoother` holds a `total` (the actual sum of all deltas added so far) and
//! an `estimate` that lags behind `total`, converging at a rate set by the
//! `e_folding` time constant. After `t` seconds the gap closes by a factor of
//! `1 - exp(-t / e_folding)`.
//!
//! Time is supplied **explicitly** as a [`Duration`] from
//! [`moonpool_core::TimeProvider::now`] so the simulation drives it
//! deterministically — no hidden `Instant::now()` calls.

use std::time::Duration;

/// Exponential moving average over an arbitrary running total.
///
/// See the module docs for the math. The smoother is `Clone` because it holds
/// only plain data, but it is **not** `Default` — `e_folding` has no sensible
/// zero value.
#[derive(Debug, Clone)]
pub struct Smoother {
    e_folding: Duration,
    total: f64,
    /// Wall-clock-equivalent time of the most recent update, in seconds since
    /// the simulation epoch (i.e. `TimeProvider::now().as_secs_f64()`).
    time_secs: f64,
    estimate: f64,
}

impl Smoother {
    /// Create a new smoother with the given e-folding time constant.
    ///
    /// `start` is the current simulation time (typically
    /// `time_provider.now()`). The initial `total` and `estimate` are both 0.
    #[must_use]
    pub fn new(e_folding: Duration, start: Duration) -> Self {
        Self {
            e_folding,
            total: 0.0,
            time_secs: start.as_secs_f64(),
            estimate: 0.0,
        }
    }

    /// Create a smoother whose `total` and `estimate` start at `initial`.
    #[must_use]
    pub fn with_initial(e_folding: Duration, start: Duration, initial: f64) -> Self {
        Self {
            e_folding,
            total: initial,
            time_secs: start.as_secs_f64(),
            estimate: initial,
        }
    }

    /// E-folding time constant for this smoother.
    #[must_use]
    pub fn e_folding(&self) -> Duration {
        self.e_folding
    }

    /// True running total (sum of all `add_delta` calls).
    #[must_use]
    pub fn total(&self) -> f64 {
        self.total
    }

    /// Add `delta` to the running total at simulation time `now`.
    ///
    /// Lazily advances the smoothed estimate up to `now` first.
    pub fn add_delta(&mut self, delta: f64, now: Duration) {
        self.advance_to(now);
        self.total += delta;
    }

    /// Smoothed total at simulation time `now`.
    ///
    /// Lazily advances the internal estimate; takes `&mut self` so the laziness
    /// is observable. Mirrors FDB `Smoother::smoothTotal`.
    pub fn smooth_total(&mut self, now: Duration) -> f64 {
        self.advance_to(now);
        self.estimate
    }

    /// Smoothed instantaneous rate of change of the total.
    ///
    /// Approximates `d/dt[smooth_total]` as `(total - estimate) / e_folding`.
    /// Returns 0 when `e_folding` is zero (defensive — `new` won't accept it
    /// in practice).
    #[must_use]
    pub fn smooth_rate(&self) -> f64 {
        let e = self.e_folding.as_secs_f64();
        if e <= 0.0 {
            0.0
        } else {
            (self.total - self.estimate) / e
        }
    }

    fn advance_to(&mut self, now: Duration) {
        let now_secs = now.as_secs_f64();
        let elapsed = now_secs - self.time_secs;
        if elapsed > 0.0 {
            let e = self.e_folding.as_secs_f64();
            if e > 0.0 {
                let factor = 1.0 - (-elapsed / e).exp();
                self.estimate += (self.total - self.estimate) * factor;
            } else {
                // Degenerate: zero e-folding ⇒ instant convergence.
                self.estimate = self.total;
            }
            self.time_secs = now_secs;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn fresh_smoother_is_zero() {
        let mut s = Smoother::new(secs(1.0), secs(0.0));
        assert_eq!(s.total(), 0.0);
        assert_eq!(s.smooth_total(secs(0.0)), 0.0);
        assert_eq!(s.smooth_rate(), 0.0);
    }

    #[test]
    fn add_delta_updates_total_immediately() {
        let mut s = Smoother::new(secs(1.0), secs(0.0));
        s.add_delta(10.0, secs(0.0));
        assert_eq!(s.total(), 10.0);
        // No time has passed → estimate still 0.
        assert_eq!(s.smooth_total(secs(0.0)), 0.0);
    }

    #[test]
    fn estimate_converges_toward_total() {
        let mut s = Smoother::new(secs(1.0), secs(0.0));
        s.add_delta(100.0, secs(0.0));

        // After exactly one e-folding the gap should close by (1 - 1/e) ≈ 0.632.
        let after_one_efold = s.smooth_total(secs(1.0));
        let expected = 100.0 * (1.0 - (-1.0_f64).exp());
        assert!(
            (after_one_efold - expected).abs() < 1e-9,
            "got {after_one_efold}, expected {expected}",
        );

        // After many e-foldings the estimate should be effectively the total.
        let after_long = s.smooth_total(secs(20.0));
        assert!((after_long - 100.0).abs() < 1e-6);
    }

    #[test]
    fn rate_approximates_derivative() {
        let mut s = Smoother::new(secs(2.0), secs(0.0));
        s.add_delta(10.0, secs(0.0));
        // rate = (total - estimate) / e_folding = 10 / 2 = 5
        assert!((s.smooth_rate() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn negative_delta_decays_back() {
        let mut s = Smoother::new(secs(1.0), secs(0.0));
        s.add_delta(50.0, secs(0.0));
        // Let the estimate climb most of the way.
        let _ = s.smooth_total(secs(10.0));
        s.add_delta(-50.0, secs(10.0));
        assert_eq!(s.total(), 0.0);
        // Estimate now decays toward 0.
        let later = s.smooth_total(secs(20.0));
        assert!(later.abs() < 1.0);
    }

    #[test]
    fn with_initial_starts_estimate_at_initial() {
        let mut s = Smoother::with_initial(secs(1.0), secs(0.0), 5.0);
        assert_eq!(s.total(), 5.0);
        assert_eq!(s.smooth_total(secs(0.0)), 5.0);
        // No drift if no deltas.
        assert_eq!(s.smooth_total(secs(100.0)), 5.0);
    }

    #[test]
    fn time_going_backwards_is_a_noop() {
        let mut s = Smoother::new(secs(1.0), secs(10.0));
        s.add_delta(100.0, secs(10.0));
        let v1 = s.smooth_total(secs(15.0));
        // Going backwards in time shouldn't change anything.
        let v2 = s.smooth_total(secs(5.0));
        assert_eq!(v1, v2);
    }

    #[test]
    fn zero_e_folding_collapses_immediately() {
        let mut s = Smoother::new(secs(0.0), secs(0.0));
        s.add_delta(42.0, secs(0.0));
        // Any forward time tick collapses estimate onto total.
        assert_eq!(s.smooth_total(secs(0.001)), 42.0);
    }
}
