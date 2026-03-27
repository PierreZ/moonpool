//! Custom transport-specific simulation report.

/// Per-workload statistics collected during simulation.
#[derive(Debug, Clone, Default)]
pub struct WorkloadStats {
    /// Fire-and-forget sends.
    pub fire_and_forget_sent: u64,
    /// Fire-and-forget errors.
    pub fire_and_forget_errors: u64,

    /// At-most-once sends.
    pub at_most_once_sent: u64,
    /// At-most-once successful replies.
    pub at_most_once_replied: u64,
    /// At-most-once MaybeDelivered outcomes.
    pub at_most_once_maybe: u64,
    /// At-most-once errors.
    pub at_most_once_errors: u64,

    /// At-least-once sends.
    pub at_least_once_sent: u64,
    /// At-least-once successful replies.
    pub at_least_once_replied: u64,
    /// At-least-once errors.
    pub at_least_once_errors: u64,
    /// At-least-once still in-flight at shutdown.
    pub at_least_once_in_flight: u64,

    /// Timeout sends.
    pub timeout_sent: u64,
    /// Timeout successful replies.
    pub timeout_replied: u64,
    /// Timeout MaybeDelivered (timed out).
    pub timeout_timed_out: u64,
    /// Timeout errors.
    pub timeout_errors: u64,

    /// Wrong endpoint sends that returned error.
    pub endpoint_not_found: u64,
}

impl std::fmt::Display for WorkloadStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "  fire_and_forget: sent={} errors={}",
            self.fire_and_forget_sent, self.fire_and_forget_errors
        )?;
        writeln!(
            f,
            "  at_most_once:    sent={} replied={} maybe={} errors={}",
            self.at_most_once_sent,
            self.at_most_once_replied,
            self.at_most_once_maybe,
            self.at_most_once_errors
        )?;
        writeln!(
            f,
            "  at_least_once:   sent={} replied={} errors={} in_flight={}",
            self.at_least_once_sent,
            self.at_least_once_replied,
            self.at_least_once_errors,
            self.at_least_once_in_flight
        )?;
        writeln!(
            f,
            "  timeout:         sent={} replied={} timed_out={} errors={}",
            self.timeout_sent, self.timeout_replied, self.timeout_timed_out, self.timeout_errors
        )?;
        writeln!(f, "  endpoint_not_found: {}", self.endpoint_not_found)?;
        Ok(())
    }
}
