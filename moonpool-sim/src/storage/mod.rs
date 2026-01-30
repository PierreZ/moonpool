//! Storage simulation and configuration.
//!
//! This module provides simulated storage that integrates with the
//! deterministic simulation engine for testing disk I/O patterns and faults.

/// Storage configuration and settings
pub mod config;

// Re-export configuration
pub use config::StorageConfiguration;
