//! Operation alphabet for spacesim workloads.
//!
//! Defines the set of operations that can be randomly generated and executed
//! against space station actors. Weights control the distribution.

use crate::RandomProvider;
use moonpool_sim::SimRandomProvider;

/// Available commodities for cargo operations.
const COMMODITIES: &[&str] = &["fuel", "ore", "food", "water"];

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
    /// Add cargo to a station.
    AddCargo {
        /// Target station name.
        station: String,
        /// Commodity type.
        commodity: String,
        /// Amount to add.
        amount: i64,
    },
    /// Remove cargo from a station.
    RemoveCargo {
        /// Target station name.
        station: String,
        /// Commodity type.
        commodity: String,
        /// Amount to remove.
        amount: i64,
    },
    /// Query the current state of a station.
    QueryState {
        /// Target station name.
        station: String,
    },
    /// Verify all stations match the reference model.
    VerifyAll,
    /// Small delay to let background tasks run.
    SmallDelay,
}

/// Generate a random operation from the alphabet.
///
/// Weights: deposit 20%, withdraw 15%, add_cargo 15%, remove_cargo 15%,
/// query 15%, verify_all 5%, delay 15%.
pub fn random_op(random: &SimRandomProvider, stations: &[String]) -> SpaceOp {
    let roll = random.random_range(0..100);
    let station = || stations[random.random_range(0..stations.len())].clone();
    let commodity = || COMMODITIES[random.random_range(0..COMMODITIES.len())].to_string();

    match roll {
        0..20 => SpaceOp::Deposit {
            station: station(),
            amount: random.random_range(1..500),
        },
        20..35 => SpaceOp::Withdraw {
            station: station(),
            amount: random.random_range(1..300),
        },
        35..50 => SpaceOp::AddCargo {
            station: station(),
            commodity: commodity(),
            amount: random.random_range(1..200),
        },
        50..65 => SpaceOp::RemoveCargo {
            station: station(),
            commodity: commodity(),
            amount: random.random_range(1..150),
        },
        65..80 => SpaceOp::QueryState { station: station() },
        80..85 => SpaceOp::VerifyAll,
        _ => SpaceOp::SmallDelay,
    }
}
