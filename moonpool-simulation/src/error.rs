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
    /// An I/O error occurred during simulation.
    #[error("I/O error: {0}")]
    IoError(String),
}

/// A type alias for `Result<T, SimulationError>`.
pub type SimulationResult<T> = Result<T, SimulationError>;

impl From<std::io::Error> for SimulationError {
    fn from(err: std::io::Error) -> Self {
        SimulationError::IoError(err.to_string())
    }
}
