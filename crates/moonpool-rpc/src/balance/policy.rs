//! Plain-data policies: what a balanced call is *permitted* to do (retry,
//! duplicate) and how the client paces itself (backoff, waits).
//!
//! Retry and duplication are separate permissions on purpose. In
//! `FoundationDB`'s `loadBalance`, `AtMostOnce` only stops the sequential
//! retry after `request_maybe_delivered`; the budgeted second request is
//! still sent. Here no retry setting can ever grant a concurrent copy:
//! copies need [`Duplicates::Permitted`], and the hedge timing lives inside
//! that permission.

use std::time::Duration;

use super::locality::Locality;
use crate::config::InvalidConfig;

/// Sequential retry permission after an attempt that may have executed.
///
/// Failing over after an attempt that provably never reached a handler
/// ([`Execution::NotAdmitted`](crate::Execution::NotAdmitted): refused
/// before admission, not connected, a stale reference) or that the
/// application's classifier declared without effect is always allowed:
/// it cannot execute the request twice. This only decides what happens
/// after an ambiguous attempt ([`Execution::MaybeExecuted`](crate::Execution::MaybeExecuted)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retry {
    /// Stop at the first ambiguous attempt and report it (the default,
    /// `FoundationDB`'s `AtMostOnce::True`). Never grants a concurrent
    /// copy.
    #[default]
    AtMostOnce,
    /// Retry on another alternative after an ambiguous attempt: the
    /// request **may execute more than once** (the application says its
    /// effect is idempotent).
    AfterAmbiguous,
}

/// When to send a hedge: `FoundationDB`'s second-request timing, measured
/// on provider time from the queue model.
///
/// The hedge goes out after `multiplier × latency(second choice) + base`,
/// or at once when the first choice's measured latency is already more
/// than `instant_factor` times that. `multiplier` is the model's adaptive
/// second-request multiplier (it grows with every hedge and decays with
/// every first response; see [`ModelConfig`](super::ModelConfig)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HedgeTiming {
    /// Added to the scaled latency (`BASE_SECOND_REQUEST_TIME`, 0.5 ms).
    pub base: Duration,
    /// Hedge at once when the first choice is this many times slower
    /// (`INSTANT_SECOND_REQUEST_MULTIPLIER`, 2).
    pub instant_factor: f64,
}

impl Default for HedgeTiming {
    fn default() -> Self {
        Self {
            base: Duration::from_micros(500),
            instant_factor: 2.0,
        }
    }
}

/// What concurrent copies a permitted call may send.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuplicatePolicy {
    /// Extra copies per call beyond the first attempt, hedges and
    /// hook-requested comparison copies together. Each also consumes one
    /// unit of the model's shared hedge budget.
    pub max_copies: u32,
    /// Hedge a slow first attempt; `None` sends no hedge (comparison copies
    /// only).
    pub hedge: Option<HedgeTiming>,
    /// How long a winner waits for its comparison copy before the
    /// comparison hook sees it as missing.
    pub compare_within: Duration,
}

impl Default for DuplicatePolicy {
    /// One hedge with the default timing.
    fn default() -> Self {
        Self {
            max_copies: 1,
            hedge: Some(HedgeTiming::default()),
            compare_within: Duration::from_secs(1),
        }
    }
}

/// Concurrent-copy permission.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Duplicates {
    /// One attempt in flight at a time (the default).
    #[default]
    Forbidden,
    /// Concurrent copies within the policy and the shared budget: the
    /// request **may execute on several servers**.
    Permitted(DuplicatePolicy),
}

impl Duplicates {
    /// Hedging with the default timing and one extra copy.
    #[must_use]
    pub fn hedged() -> Self {
        Self::Permitted(DuplicatePolicy::default())
    }

    /// The copy policy, if copies are permitted.
    #[must_use]
    pub fn policy(&self) -> Option<&DuplicatePolicy> {
        match self {
            Self::Forbidden => None,
            Self::Permitted(policy) => Some(policy),
        }
    }
}

/// Per-call permissions and bounds.
///
/// The default permits neither a retry after an ambiguous attempt nor a
/// concurrent copy: at most one attempt can have reached a handler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BalancePolicy {
    /// Sequential retry after an ambiguous attempt.
    pub retry: Retry,
    /// Concurrent copies (hedges, comparison copies).
    pub duplicates: Duplicates,
    /// Sequential attempts per call (first attempt and fail-overs; copies
    /// are bounded by [`DuplicatePolicy::max_copies`]). At least one.
    pub max_attempts: u32,
    /// Give up on one attempt after this long (it ends
    /// [`ErrorReason::Timeout`](crate::ErrorReason::Timeout), ambiguous
    /// once sent). `None`: wait for its outcome.
    pub attempt_timeout: Option<Duration>,
}

impl Default for BalancePolicy {
    fn default() -> Self {
        Self {
            retry: Retry::AtMostOnce,
            duplicates: Duplicates::Forbidden,
            max_attempts: 4,
            attempt_timeout: None,
        }
    }
}

impl BalancePolicy {
    /// The default: fail over only when nothing can have executed.
    #[must_use]
    pub fn at_most_once() -> Self {
        Self::default()
    }

    /// Retry after ambiguous attempts; still no concurrent copy.
    #[must_use]
    pub fn idempotent() -> Self {
        Self {
            retry: Retry::AfterAmbiguous,
            ..Self::default()
        }
    }

    /// Hedge slow attempts (concurrent copies); still no sequential retry
    /// after an ambiguous attempt.
    #[must_use]
    pub fn hedged() -> Self {
        Self {
            duplicates: Duplicates::hedged(),
            ..Self::default()
        }
    }
}

/// A bounded, growing, jittered delay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Backoff {
    /// The first delay.
    pub initial: Duration,
    /// The longest delay.
    pub max: Duration,
    /// Growth factor per step (at least 1).
    pub growth: f64,
    /// Fraction of each delay drawn at random from the provider RNG and
    /// taken off (0: none, 1: anywhere from zero to the full delay).
    pub jitter: f64,
}

impl Backoff {
    /// The step after `current` (`initial` when there was none).
    #[must_use]
    pub fn next(&self, current: Duration) -> Duration {
        if current.is_zero() {
            return self.initial.min(self.max);
        }
        let grown = current.mul_f64(self.growth.max(1.0));
        grown.max(self.initial).min(self.max)
    }

    /// `delay` less a random share of `jitter`; `draw` is uniform in
    /// `0.0..1.0`.
    #[must_use]
    pub fn jittered(&self, delay: Duration, draw: f64) -> Duration {
        let jitter = self.jitter.clamp(0.0, 1.0) * draw.clamp(0.0, 1.0);
        delay.mul_f64(1.0 - jitter)
    }

    fn is_valid(&self) -> bool {
        self.growth.is_finite()
            && self.growth >= 1.0
            && self.jitter.is_finite()
            && (0.0..=1.0).contains(&self.jitter)
            && self.initial <= self.max
    }
}

/// Client-wide configuration of a balanced client.
#[derive(Debug, Clone, PartialEq)]
pub struct BalanceConfig {
    /// The caller's own locality, ranked against each alternative's.
    pub locality: Locality,
    /// Pause before each new round once every usable alternative was
    /// tried in the call (`LOAD_BALANCE_{START,MAX}_BACKOFF`,
    /// `LOAD_BALANCE_BACKOFF_RATE`: 10 ms doubling to 5 s), jittered.
    pub round_backoff: Backoff,
    /// How long a call waits for any alternative to become reachable when
    /// none is, before it fails with
    /// [`BalanceFailure::AllAlternativesFailed`](super::BalanceFailure::AllAlternativesFailed)
    /// (`ALTERNATIVES_FAILURE_MAX_DELAY`, 1 s), jittered like
    /// `round_backoff`.
    pub all_failed_wait: Duration,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            locality: Locality::unknown(),
            round_backoff: Backoff {
                initial: Duration::from_millis(10),
                max: Duration::from_secs(5),
                growth: 2.0,
                jitter: 0.5,
            },
            all_failed_wait: Duration::from_secs(1),
        }
    }
}

impl BalanceConfig {
    /// Check the configuration.
    ///
    /// # Errors
    ///
    /// A description of the first invalid field.
    pub fn validate(&self) -> Result<(), InvalidConfig> {
        validate_backoff("round_backoff", &self.round_backoff)
    }
}

pub(crate) fn validate_backoff(name: &str, backoff: &Backoff) -> Result<(), InvalidConfig> {
    if backoff.is_valid() {
        Ok(())
    } else {
        Err(InvalidConfig(format!(
            "{name}: growth must be >= 1, jitter in 0..=1, initial <= max"
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Backoff, BalancePolicy, Duplicates, Retry};

    #[test]
    fn defaults_permit_neither_retry_nor_copies() {
        let policy = BalancePolicy::default();
        assert_eq!(policy.retry, Retry::AtMostOnce);
        assert_eq!(policy.duplicates, Duplicates::Forbidden);
        assert!(policy.duplicates.policy().is_none());
        // The retry permission never carries a copy permission.
        let idempotent = BalancePolicy::idempotent();
        assert!(idempotent.duplicates.policy().is_none());
        let hedged = BalancePolicy::hedged();
        assert_eq!(hedged.retry, Retry::AtMostOnce);
        assert!(hedged.duplicates.policy().is_some());
    }

    #[test]
    fn backoff_grows_to_its_bound_and_jitter_only_shortens() {
        let backoff = Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(50),
            growth: 2.0,
            jitter: 0.5,
        };
        let mut delay = Duration::ZERO;
        let mut seen = Vec::new();
        for _ in 0..5 {
            delay = backoff.next(delay);
            seen.push(delay.as_millis());
        }
        assert_eq!(seen, vec![10, 20, 40, 50, 50]);
        assert_eq!(
            backoff.jittered(Duration::from_millis(40), 0.0).as_millis(),
            40
        );
        assert_eq!(
            backoff.jittered(Duration::from_millis(40), 1.0).as_millis(),
            20
        );
        assert!(backoff.jittered(Duration::from_millis(40), 7.0) >= Duration::from_millis(20));
    }
}
