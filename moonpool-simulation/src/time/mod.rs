//! Time provider abstraction for simulation and real time.
//!
//! This module provides a unified interface for time operations that works
//! seamlessly with both simulation time and real wall-clock time.

/// Core time provider trait implementations
pub mod provider;

// Re-export main types
pub use provider::{SimTimeProvider, TimeProvider, TokioTimeProvider};
