//! Chaos testing infrastructure for deterministic fault injection.
//!
//! This module provides the testing utilities inspired by FoundationDB's
//! buggify approach for comprehensive chaos testing of distributed systems.
//!
//! ## Submodules
//!
//! - [`assertions`] - `always_assert!` and `sometimes_assert!` macros for testing
//! - [`buggify`] - Deterministic fault injection via `buggify!` macro
//! - [`invariants`] - Cross-workload invariant checking
//! - [`state_registry`] - Actor state observability for debugging
//!
//! ## Philosophy
//!
//! The goal is to find bugs through comprehensive chaos testing:
//! - **always_assert!**: Guard invariants (must never fail)
//! - **sometimes_assert!**: Test error paths (statistical coverage)
//! - **buggify!**: Deterministic fault injection (25% probability when enabled)
//! - **Invariants**: Cross-actor properties validated after every event

pub mod assertions;
pub mod buggify;
pub mod invariants;
pub mod state_registry;

// Re-export main types at module level
pub use assertions::{
    AssertionStats, get_assertion_results, panic_on_assertion_violations, record_assertion,
    reset_assertion_results, validate_assertion_contracts,
};
pub use buggify::{buggify_init, buggify_internal, buggify_reset};
pub use invariants::InvariantCheck;
pub use state_registry::StateRegistry;
