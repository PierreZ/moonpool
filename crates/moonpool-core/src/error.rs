//! Error types for simulation operations.

use thiserror::Error;

/// Errors that can occur during simulation operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SimulationError {
    /// The simulation has been shut down and is no longer accessible.
    #[error("Simulation has been shut down")]
    SimulationShutdown,
    /// The simulation is in an invalid state.
    #[error("Invalid simulation state: {0}")]
    InvalidState(String),
    /// The simulation was configured in a way that cannot run as written,
    /// detected before any seed runs.
    #[error("Invalid simulation configuration: {0}")]
    InvalidConfiguration(String),
    /// An I/O error occurred during simulation.
    #[error("I/O error: {0}")]
    IoError(String),
    /// A stall guard of the runner breached: the seed made no progress, or
    /// ran past its virtual-time budget. The runner triggered shutdown so
    /// that the seed ends, and the seed fails.
    #[error("Stall: {0}")]
    Stall(String),
    /// The simulation did not settle within the timeout after shutdown.
    #[error("Settle timeout: {pending_events} events still pending after {elapsed:?}")]
    SettleTimeout {
        /// Number of events still pending when timeout was reached.
        pending_events: usize,
        /// How long the settle phase ran before timing out.
        elapsed: std::time::Duration,
    },
}

/// A type alias for `Result<T, SimulationError>`.
pub type SimulationResult<T> = Result<T, SimulationError>;

impl From<std::io::Error> for SimulationError {
    fn from(err: std::io::Error) -> Self {
        SimulationError::IoError(err.to_string())
    }
}
