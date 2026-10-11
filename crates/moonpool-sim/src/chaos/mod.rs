//! Fault injection and assertion accounting.
//!
//! `FoundationDB`'s lesson: **bugs hide in error paths**. Production rarely
//! runs a timeout handler, a retry loop or a recovery path. This module holds
//! the code-side half of moonpool's chaos: the fault sites inside the code
//! under test, and the assertions that prove those paths ran.
//!
//! The environment-side half (network, storage, reboots, outages, slow
//! machines) is turned on with
//! [`SimulationBuilder::enable_chaos`](crate::SimulationBuilder::enable_chaos)
//! and documented on [`Chaos`](crate::Chaos).
//!
//! # Submodules
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`assertions`] | The fifteen `assert_*` macros and their result tables |
//! | [`buggify`] | Simulation wiring for `moonpool-buggify` and the [`buggify_knob!`](crate::buggify_knob) macro |
//! | [`fault_events`] | [`SimFaultEvent`]: every injected fault, on the trace timeline |
//! | [`state_handle`] | [`StateHandle`]: typed values shared inside one run |
//! | [`exploration_glue`] | The thin layer over the optional `moonpool-explorer` backend |
//!
//! # Buggify
//!
//! The buggify macros live in the `moonpool-buggify` crate, which has no
//! dependencies, so shipped code can use them. This crate re-exports them.
//! Outside a simulation every site returns `false` and draws nothing.
//!
//! ```rust
//! use moonpool_sim::{buggify, buggify_with_prob};
//!
//! fn send_batch(batch: &[u8]) -> Result<usize, &'static str> {
//!     // A rare-but-legal path: send only part of the batch.
//!     if buggify!() {
//!         return Ok(batch.len() / 2);
//!     }
//!     // A site with its own firing rate.
//!     if buggify_with_prob!(0.02) {
//!         return Err("simulated transient failure");
//!     }
//!     Ok(batch.len())
//! }
//!
//! // Outside a simulation, no site fires.
//! assert_eq!(send_batch(b"abcd"), Ok(4));
//! ```
//!
//! Each site decides in two phases:
//!
//! 1. **Activation**, once per site per seed: the site is active with
//!    probability 50% (the runner calls [`buggify_init`]`(0.5)`).
//! 2. **Firing**, on each call: an active site fires at its rate, 25% for
//!    [`buggify!`](crate::buggify), the argument for
//!    [`buggify_with_prob!`](crate::buggify_with_prob).
//!
//! So one seed sees a stable set of active sites, and different seeds see
//! different sets. `FoundationDB` uses the same model (`Buggify.h`).
//!
//! The other sites:
//!
//! | Macro | Use it for |
//! |-------|------------|
//! | [`buggify_fault_with_prob!`](crate::buggify_fault_with_prob) | A site that makes an operation fail. Silent after the chaos window, so the recovery tail stays quiet. |
//! | [`buggify_named!`](crate::buggify_named) | A disruptive site with a stable label. The harness decides its activation per seed with [`set_activation`](crate::set_activation). |
//! | [`buggify_pick!`](crate::buggify_pick) | Override a choice: `Some(i)` in `0..n` when it fires |
//! | [`buggify_range!`](crate::buggify_range) | Stretch a value: `Some(v)` in a range when it fires |
//! | [`buggify_knob!`](crate::buggify_knob) | A configuration value that jumps to an extreme on some seeds |
//! | [`hint!`](crate::hint!) | Name a moment; the simulator may reboot the process there (see [`HintVeto`](crate::HintVeto)) |
//! | [`cpu_charge`](crate::cpu_charge) | Tell the CPU gate that work costs CPU time |
//!
//! Place sites at error paths, timeout boundaries, retry entry points,
//! resource limits and state transitions.
//!
//! # Assertions
//!
//! Assertions never panic. A failed `always` assertion is logged and
//! counted; the runner fails the seed after the iteration ([`has_always_violations`]).
//! A `sometimes` assertion that never fires on any seed is a coverage
//! violation in the report.
//!
//! ```rust
//! use moonpool_sim::{assert_always, assert_sometimes};
//!
//! let (sent, received) = (10_u64, 9_u64);
//! // Safety: must hold every time it is checked.
//! assert_always!(received <= sent, "never receive more than was sent");
//! // Coverage: must be true at least once across the campaign.
//! assert_sometimes!(received < sent, "a message was lost in flight");
//! ```
//!
//! With the default stop rule (`UntilCoverageStable`), the runner keeps
//! trying seeds until every `sometimes` and `reachable` assertion fired and
//! code coverage stopped growing.
//!
//! # The chaos window and the recovery tail
//!
//! Every fault source is bounded by
//! [`SimulationBuilder::chaos_duration`](crate::SimulationBuilder::chaos_duration).
//! When it ends, the runner calls
//! [`SimWorld::enter_recovery_mode`](crate::SimWorld::enter_recovery_mode):
//! fault injectors stop, every configuration-driven network, storage and
//! block-device fault family turns off, the partitions the simulator holds
//! heal, and `buggify_fault_with_prob!` sites go silent.
//!
//! The boundary stops **fault generation** only. It repairs nothing:
//! corrupted sectors, lost writes, closed connections and killed processes
//! stay. Finite effects already running expire on their own schedule. The
//! system under test must recover by itself in the quiet tail.

pub mod assertions;
pub mod buggify;
pub mod exploration_glue;
pub mod fault_events;
pub mod state_handle;

// Re-export main types at module level
pub use assertions::{
    AssertionStats, assertion_results, has_always_violations, on_assertion_bool,
    on_assertion_numeric, on_assertion_sometimes_all, on_sometimes_each, record_always_violation,
    reset_always_violations, reset_assertion_results, validate_assertion_contracts,
};
pub use buggify::{buggify_init, buggify_internal, buggify_knob_internal, buggify_reset};
pub use fault_events::{SIM_FAULT_EVENT_NAME, SimFaultEvent};
pub use state_handle::StateHandle;
