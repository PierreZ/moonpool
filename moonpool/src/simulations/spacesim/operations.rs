//! Operation alphabet for spacesim workloads.
//!
//! Defines the set of operations that can be randomly generated and executed
//! against space station actors. Weights control the distribution.

use crate::RandomProvider;
use moonpool_sim::SimRandomProvider;

/// An operation in the spacesim alphabet.
pub enum SpaceOp {
    /// Deposit credits into a station.
    Deposit {
        /// Target station name.
        station: String,
        /// Amount to deposit.
        amount: i64,
    },
    /// Withdraw credits from a station.
    Withdraw {
        /// Target station name.
        station: String,
        /// Amount to withdraw.
        amount: i64,
    },
    /// Query the current state of a station.
    QueryState {
        /// Target station name.
        station: String,
    },
    /// Small delay to let background tasks run.
    SmallDelay,
}

/// Generate a random operation from the alphabet.
///
/// Weights: deposit 30%, withdraw 25%, query 25%, delay 20%.
pub fn random_op(random: &SimRandomProvider, stations: &[String]) -> SpaceOp {
    let roll = random.random_range(0..100);
    let station = || stations[random.random_range(0..stations.len())].clone();

    match roll {
        0..30 => SpaceOp::Deposit {
            station: station(),
            amount: random.random_range(1..500),
        },
        30..55 => SpaceOp::Withdraw {
            station: station(),
            amount: random.random_range(1..300),
        },
        55..80 => SpaceOp::QueryState { station: station() },
        _ => SpaceOp::SmallDelay,
    }
}
