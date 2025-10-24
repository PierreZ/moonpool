//! Placement strategies for actor node selection.
//!
//! This module is responsible for choosing which node should host a new actor activation.
//! It is separate from the directory (which tracks where actors currently are).

pub mod simple;

// Re-exports
pub use simple::SimplePlacement;
