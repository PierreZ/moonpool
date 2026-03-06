//! Operation alphabet for spacesim workloads.
//!
//! Defines the set of operations that can be randomly generated and executed
//! against space station actors. Weights control the distribution.

use crate::RandomProvider;
use moonpool_sim::SimRandomProvider;

use super::actors::TradeDirection;

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
    /// Execute a trade between a ship and a station.
    Trade {
        /// Ship performing the trade.
        ship: String,
        /// Target station.
        station: String,
        /// Commodity to trade.
        commodity: String,
        /// Amount of commodity.
        amount: i64,
        /// Price in credits.
        price: i64,
        /// Buy or sell direction.
        direction: TradeDirection,
    },
    /// Query the current state of a ship.
    QueryShip {
        /// Target ship name.
        ship: String,
    },
    /// Verify all stations and ships match the reference model.
    VerifyAll,
    /// Small delay to let background tasks run.
    SmallDelay,
}

/// Generate a random operation from the alphabet.
///
/// Weights: deposit 15%, withdraw 10%, add_cargo 10%, remove_cargo 10%,
/// query_state 10%, trade 20%, query_ship 5%, verify_all 5%, delay 15%.
pub fn random_op(random: &SimRandomProvider, stations: &[String], ships: &[String]) -> SpaceOp {
    let roll = random.random_range(0..100);
    let station = || stations[random.random_range(0..stations.len())].clone();
    let ship = || ships[random.random_range(0..ships.len())].clone();
    let commodity = || COMMODITIES[random.random_range(0..COMMODITIES.len())].to_string();

    match roll {
        0..15 => SpaceOp::Deposit {
            station: station(),
            amount: random.random_range(1..500),
        },
        15..25 => SpaceOp::Withdraw {
            station: station(),
            amount: random.random_range(1..300),
        },
        25..35 => SpaceOp::AddCargo {
            station: station(),
            commodity: commodity(),
            amount: random.random_range(1..200),
        },
        35..45 => SpaceOp::RemoveCargo {
            station: station(),
            commodity: commodity(),
            amount: random.random_range(1..150),
        },
        45..55 => SpaceOp::QueryState { station: station() },
        55..75 => SpaceOp::Trade {
            ship: ship(),
            station: station(),
            commodity: commodity(),
            amount: random.random_range(1..100),
            price: random.random_range(1..200),
            direction: if random.random_range(0..2) == 0 {
                TradeDirection::Buy
            } else {
                TradeDirection::Sell
            },
        },
        75..80 => SpaceOp::QueryShip { ship: ship() },
        80..85 => SpaceOp::VerifyAll,
        _ => SpaceOp::SmallDelay,
    }
}
