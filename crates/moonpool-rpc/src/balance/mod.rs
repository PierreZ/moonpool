//! Load balancing over explicit alternative sets, with separate retry and
//! duplicate permissions (`FoundationDB`'s `loadBalance`, `QueueModel` and
//! `MultiInterface`, adapted).
//!
//! ```text
//!  AlternativeSet<M> v7 ──replace(v8)──► BalancedClient<P, M>
//!   [ServiceRef I1 @dc1, I2 @dc1, I3 @dc2]      │ call(request, &policy)
//!                                               ▼
//!        Selector (QueueSelector default) ◄─ QueueModel ◄─ reservations,
//!          │  order by locality, work, penalty     latency, exclusions,
//!          ▼                                        hedge budget, late losers
//!        attempt ─► ReplyAttempt ─► winner completes the caller
//!          └─ hedge / comparison copy (only with Duplicates::Permitted)
//! ```
//!
//! - **Alternatives are data.** An [`AlternativeSet`] holds typed,
//!   incarnation-specific [`ServiceRef`](crate::ServiceRef)s and an
//!   application-chosen [`SetVersion`]. It is never refreshed by the
//!   balancer: a restarted server is a new reference in a new set,
//!   installed with [`BalancedClient::replace`] (strictly newer versions
//!   only). A running call keeps the set it started with.
//! - **Permissions are plain data.** [`BalancePolicy::retry`] decides
//!   whether an ambiguous attempt may be followed by another one;
//!   [`BalancePolicy::duplicates`] whether concurrent copies (hedges,
//!   comparison copies) may exist. They are independent, and the default
//!   grants neither. A fail-over after an attempt that provably never
//!   reached a handler needs no permission.
//! - **Every attempt reserves once and releases once** in the shared
//!   [`QueueModel`], whichever way it ends: winner, error, declined,
//!   cancelled caller, late loser, dropped late loser.
//! - **Errors keep every attempt's ambiguity**: a [`BalanceError`] carries
//!   each attempt and the strongest execution knowledge over them.
//!
//! Late losers are driven by [`QueueModel::collect_lagging`], polled next
//! to the application like the RPC driver.

mod client;
mod hooks;
mod locality;
mod model;
mod policy;
mod set;

pub use client::{
    AttemptOutcome, AttemptRecord, BalanceError, BalanceFailure, Balanced, BalancedClient,
};
pub use hooks::{
    AttemptEnd, AttemptEvent, AttemptKind, BalanceHooks, BusyScoreSelector, Candidate,
    DefaultHooks, QueueSelector, Selector, Verdict,
};
pub use locality::{Distance, Locality};
pub use model::{
    Feedback, HedgeBudget, Measurement, ModelConfig, ModelOutcome, ModelStats, QueueModel,
    Reservation,
};
pub use policy::{
    Backoff, BalanceConfig, BalancePolicy, DuplicatePolicy, Duplicates, HedgeTiming, Retry,
};
pub use set::{Alternative, AlternativeSet, InvalidSet, ReplaceError, SetStatus, SetVersion};
