//! # moonpool-rpc-sim
//!
//! The deterministic-simulation side of `moonpool-rpc`: process and workload
//! definitions, independent oracles and the campaigns CI runs. It is not
//! published, and the production crate never depends on it.
//!
//! - [`foundations`]: the `sim-rpc-foundations` campaign (#213): typed
//!   dynamic-endpoint request/reply between two process groups and a
//!   surviving workload, judged by a handler receipt ledger kept apart from
//!   the transport.
//! - [`delivery`]: the `sim-rpc-delivery` campaign (#214): every delivery
//!   mode, lost replies, crashes, simultaneous connects, bootstrap names
//!   and failure-monitor transitions, judged by an execution ledger.
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod delivery;
pub mod foundations;
