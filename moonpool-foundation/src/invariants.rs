//! Cross-workload invariant checking for simulation testing.
//!
//! This module provides types for defining invariant check functions that
//! validate global distributed system properties across multiple workloads.
//!
//! # Philosophy
//!
//! Invariants implement "crash early" testing inspired by FoundationDB:
//! - Check properties after every simulation event
//! - Panic immediately when violations detected
//! - Provides detailed state for debugging
//! - Enables checking properties that span multiple actors
//!
//! # Example
//!
//! ```rust
//! use moonpool_foundation::InvariantCheck;
//! use std::collections::HashMap;
//! use serde_json::Value;
//!
//! // Invariant: Total messages received <= total messages sent
//! let message_conservation: InvariantCheck = Box::new(|states: &HashMap<String, Value>, _time: u64| {
//!     let total_sent: u64 = states.values()
//!         .filter_map(|v| v.get("messages_sent").and_then(|s| s.as_u64()))
//!         .sum();
//!     let total_received: u64 = states.values()
//!         .filter_map(|v| v.get("messages_received").and_then(|r| r.as_u64()))
//!         .sum();
//!
//!     assert!(
//!         total_received <= total_sent,
//!         "Message conservation violated: {} received but only {} sent",
//!         total_received,
//!         total_sent
//!     );
//! });
//! ```

use serde_json::Value;
use std::collections::HashMap;

/// Type alias for invariant check functions.
///
/// An invariant check is a closure that receives:
/// - `states`: HashMap mapping actor names (e.g., IP addresses) to their JSON state
/// - `sim_time_ms`: Current simulation time in milliseconds
///
/// The closure should panic if an invariant violation is detected.
/// The panic message should include relevant details for debugging.
///
/// # Invariant Design Guidelines
///
/// ## When to Use Invariants
///
/// - **Cross-actor properties**: Properties that span multiple actors
///   - Example: "At most one leader across all nodes"
///   - Example: "Total messages received <= total messages sent"
///
/// - **Global system properties**: System-wide constraints
///   - Example: "No more than N connections system-wide"
///   - Example: "All actors making progress after X time"
///
/// - **Deterministic bugs**: Catch bugs that only appear in specific orderings
///   - Example: "No deadlock detected (no actor stuck with no events)"
///   - Example: "Message ordering preserved across network"
///
/// ## When NOT to Use Invariants
///
/// - **Per-actor validation**: Use `always_assert!` in actor code instead
/// - **Expected failures**: Don't check things that are supposed to fail under chaos
/// - **Performance-sensitive checks**: Invariants run after EVERY event
///
/// ## Example Patterns
///
/// ```rust
/// # use std::collections::HashMap;
/// # use serde_json::Value;
/// # use moonpool_foundation::InvariantCheck;
///
/// // Pattern 1: Conservation law
/// let conservation: InvariantCheck = Box::new(|states, _time| {
///     let total_in: u64 = states.values()
///         .filter_map(|v| v.get("input").and_then(|i| i.as_u64()))
///         .sum();
///     let total_out: u64 = states.values()
///         .filter_map(|v| v.get("output").and_then(|o| o.as_u64()))
///         .sum();
///     assert!(total_out <= total_in, "Conservation violated");
/// });
///
/// // Pattern 2: Uniqueness constraint
/// let single_leader: InvariantCheck = Box::new(|states, _time| {
///     let leader_count = states.values()
///         .filter(|v| v.get("is_leader").and_then(|l| l.as_bool()).unwrap_or(false))
///         .count();
///     assert!(leader_count <= 1, "Split brain: {} leaders", leader_count);
/// });
///
/// // Pattern 3: Progress check (time-dependent)
/// let making_progress: InvariantCheck = Box::new(|states, time| {
///     if time > 5000 {  // After 5 seconds
///         for (name, state) in states {
///             let work = state.get("work_done").and_then(|w| w.as_u64()).unwrap_or(0);
///             if work == 0 {
///                 // Just warn, don't panic (might be intentional)
///                 eprintln!("WARNING: {} hasn't done work after {}ms", name, time);
///             }
///         }
///     }
/// });
///
/// // Pattern 4: Relationship constraint
/// let request_response_match: InvariantCheck = Box::new(|states, _time| {
///     for (name, state) in states {
///         if state.get("role").and_then(|r| r.as_str()) == Some("client") {
///             let requests = state.get("requests_sent").and_then(|r| r.as_u64()).unwrap_or(0);
///             let responses = state.get("responses_received").and_then(|r| r.as_u64()).unwrap_or(0);
///             assert!(
///                 responses <= requests,
///                 "Client {} has {} responses but only sent {} requests",
///                 name, responses, requests
///             );
///         }
///     }
/// });
/// ```
///
/// # Testing Strategy
///
/// Invariants are most powerful when combined with chaos testing:
///
/// ```ignore
/// SimulationBuilder::new()
///     .use_random_config()  // Enable network chaos
///     .with_invariants(vec![
///         message_conservation(),
///         single_leader(),
///         making_progress(),
///     ])
///     .set_iteration_control(IterationControl::UntilAllSometimesReached(1000))
///     .run()
///     .await;
/// ```
///
/// This runs the simulation with random network conditions across 1000+ seeds,
/// checking invariants after every event. If any invariant fails, you get:
/// - The exact seed that triggered the bug (reproducible)
/// - The simulation time when it occurred
/// - The complete actor states at that moment
/// - The invariant's assertion message
pub type InvariantCheck = Box<dyn Fn(&HashMap<String, Value>, u64)>;
