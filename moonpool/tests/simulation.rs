//! Virtual actor simulation tests.
//!
//! FDB-style alphabet workloads for the virtual actor system, exercising
//! actor lifecycle, state persistence, and method dispatch under chaos.

#[path = "simulation/mod.rs"]
mod simulation_mod;

pub use simulation_mod::*;
