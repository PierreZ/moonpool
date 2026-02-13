//! Chaos testing infrastructure for deterministic fault injection.
//!
//! This module implements FoundationDB's buggify approach for finding bugs
//! through comprehensive chaos testing.
//!
//! # Philosophy
//!
//! FoundationDB's insight: **bugs hide in error paths**. Production code
//! rarely exercises timeout handlers, retry logic, or failure recovery.
//! Chaos testing finds these bugs before production does.
//!
//! Key principles:
//! - **Deterministic**: Same seed produces same faults for reproducible debugging
//! - **Comprehensive**: Test all failure modes (network, timing, corruption)
//! - **Low probability**: Faults rare enough for progress, frequent enough to find bugs
//!
//! # Components
//!
//! | Component | Purpose |
//! |-----------|---------|
//! | [`buggify!`] | Probabilistic fault injection at code locations |
//! | [`assert_always!`] | Invariants that must never fail |
//! | [`assert_sometimes!`] | Behaviors that should occur under chaos |
//! | [`assert_sometimes_each!`] | Per-value bucketed sometimes assertions |
//! | [`Invariant`] | Cross-workload properties validated after events |
//! | [`StateHandle`] | Shared state for cross-workload communication |
//!
//! # The Buggify System
//!
//! Each buggify location is randomly **activated** once per simulation run,
//! then fires probabilistically on each call.
//!
//! ```ignore
//! // 25% probability when activated
//! if buggify!() {
//!     return Err(SimulatedFailure);
//! }
//!
//! // Custom probability
//! if buggify_with_prob!(0.02) {
//!     corrupt_data();
//! }
//! ```
//!
//! ## Two-Phase Activation
//!
//! 1. **Activation** (once per location per seed): `random() < activation_prob`
//! 2. **Firing** (each call): If active, `random() < firing_prob`
//!
//! This ensures consistent behavior within a run while varying which
//! locations are active across different seeds.
//!
//! | Parameter | Default | FDB Reference |
//! |-----------|---------|---------------|
//! | `activation_prob` | 25% | `Buggify.h:79-88` |
//! | `firing_prob` | 25% | `P_GENERAL_BUGGIFIED_SECTION_FIRES` |
//!
//! # Assertions
//!
//! ## assert_always!
//!
//! Guards invariants that must **never** fail:
//!
//! ```ignore
//! assert_always!(
//!     sent_count >= received_count,
//!     "received more than sent"
//! );
//! ```
//!
//! ## assert_sometimes!
//!
//! Validates that error paths **do** execute under chaos:
//!
//! ```ignore
//! if buggify!() {
//!     assert_sometimes!(true, "timeout_triggered");
//!     return Err(Timeout);
//! }
//! ```
//!
//! Multi-seed testing with `UntilAllSometimesReached(1000)` ensures all
//! `assert_sometimes!` statements fire across the seed space.
//!
//! # Strategic Placement
//!
//! Place `buggify!()` calls at:
//! - Error handling paths
//! - Timeout boundaries
//! - Retry logic entry points
//! - Resource limit checks
//! - State transitions

pub mod assertions;
pub mod buggify;
pub mod invariant_trait;
pub mod state_handle;

// Re-export main types at module level
pub use assertions::{
    AssertionStats, get_assertion_results, on_sometimes_each, on_sometimes_success,
    panic_on_assertion_violations, record_assertion, reset_assertion_results,
    validate_assertion_contracts,
};
pub use buggify::{buggify_init, buggify_internal, buggify_reset};
pub use invariant_trait::{Invariant, invariant_fn};
pub use state_handle::StateHandle;
