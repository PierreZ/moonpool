//! Recovery invariant for the metastable failure simulation.
//!
//! Models the AWS DWFM outage invariant: once the trigger (DNS) resolves,
//! the system should recover within a bounded time. Violation means the
//! system has entered congestive collapse — the defining property of
//! metastable failure.

use std::cell::Cell;

use moonpool_sim::{Invariant, StateHandle, assert_always};

/// How long DNS must be continuously healthy before we expect recovery (sim time).
const RECOVERY_DEADLINE_MS: u64 = 15_000;

/// Goodput must be at or above this threshold after the recovery deadline.
const RECOVERY_THRESHOLD: f64 = 0.2;

/// Recovery invariant: if DNS has been healthy for longer than the recovery
/// deadline, goodput must have recovered above the threshold.
///
/// In a healthy system, DNS heals → hosts renew → goodput recovers well
/// within 15s. During metastable failure, DNS heals but the thundering herd
/// sustains overload → goodput stays collapsed → invariant violated.
pub struct RecoveryInvariant {
    /// When DNS became continuously healthy (sim time ms), or None if unhealthy.
    dns_healthy_since: Cell<Option<u64>>,
}

impl RecoveryInvariant {
    /// Create a new recovery invariant.
    pub fn new() -> Self {
        Self {
            dns_healthy_since: Cell::new(None),
        }
    }
}

impl Invariant for RecoveryInvariant {
    fn name(&self) -> &str {
        "recovery"
    }

    fn check(&self, state: &StateHandle, sim_time_ms: u64) {
        let dns_healthy = state.get::<bool>("dns_healthy").unwrap_or(true);

        if dns_healthy {
            if self.dns_healthy_since.get().is_none() {
                self.dns_healthy_since.set(Some(sim_time_ms));
            }

            if let Some(since) = self.dns_healthy_since.get() {
                let healthy_for = sim_time_ms.saturating_sub(since);
                if healthy_for > RECOVERY_DEADLINE_MS {
                    let goodput = state.get::<f64>("goodput").unwrap_or(1.0);
                    assert_always!(goodput >= RECOVERY_THRESHOLD, "recovery_after_dns_heals");
                }
            }
        } else {
            self.dns_healthy_since.set(None);
        }
    }
}
