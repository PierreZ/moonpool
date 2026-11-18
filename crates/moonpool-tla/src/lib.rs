//! Model-based trace checking for moonpool: is this run of the real code a
//! behaviour that a TLA+ spec allows?
//!
//! A run records its events in order in a [`Trace`]. [`TraceCheck`] writes
//! the trace as a TLA+ module, runs TLC on a hand-written trace spec that
//! extends the real spec and follows the events one by one, and reads the
//! [`Verdict`]: accepted, rejected at the first event that does not match
//! (with the model state before it), or an invariant of the spec broken.
//!
//! The method is the one of Cirstea, Kuppe, Loillier and Merz, "Validating
//! Traces of Distributed Programs Against TLA+ Specifications" (2024). A
//! moonpool run fits it well: the simulation is one thread with one
//! sequence counter, so the timeline is already a total order of every
//! process's events, and a rejected trace is a seed that replays.
//!
//! The crate has no dependency and does not need the simulator: a sans-I/O
//! harness records events itself, and `moonpool-sim` builds a [`Trace`]
//! from its captured timeline. Running TLC needs a platform that can start
//! a process and the `tlc` launcher (nixpkgs `tlaplus`), or the path in
//! [`TLC_ENV`].
//!
//! ```no_run
//! use moonpool_tla::{Trace, TraceCheck, Value, Verdict};
//!
//! let mut trace = Trace::new();
//! trace.define("TraceRM", Value::str_set(["r1", "r2"]));
//! trace.event("rm_prepare", [("rm", Value::from("r1"))]);
//! trace.event("tm_commit", Vec::<(String, Value)>::new());
//!
//! // specs/ holds TwoPhase.tla, TwoPhaseTrace.tla and TwoPhaseTrace.cfg.
//! let verdict = TraceCheck::new("specs", "TwoPhaseTrace").check(&trace)?;
//! if let Verdict::Rejected(rejection) = verdict {
//!     println!("{rejection}"); // event 1 does not match the spec; ...
//! }
//! # Ok::<(), moonpool_tla::CheckError>(())
//! ```

#![deny(missing_docs)]

mod check;
mod trace;
mod value;

pub use check::{
    CheckError, DEFAULT_CURSOR, DEFAULT_END_INVARIANT, DEFAULT_TRACE_MODULE, Rejection, TLC_ENV,
    TraceCheck, Verdict,
};
pub use trace::{EVENT_FIELD, Trace};
pub use value::{Value, ValueError};
