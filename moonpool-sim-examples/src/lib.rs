//! Example simulation workloads for moonpool-sim.
//!
//! Contains self-contained game logic (maze, dungeon) that serves as both
//! examples and test workloads for fork-based exploration.
//!
//! Separated from `moonpool-sim` so that sancov can instrument just the
//! application code (~2-3K edges) without the simulation framework (~21K edges).

#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]

pub mod dungeon;
pub mod maze;
