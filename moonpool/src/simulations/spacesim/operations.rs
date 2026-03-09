//! Operation alphabet for the cargo hauling network.
//!
//! Operations are generated based on model state to ensure preconditions
//! are satisfiable.

use moonpool_sim::SimRandomProvider;

use crate::RandomProvider;

use super::model::SpaceModel;

/// Available commodities.
const COMMODITIES: &[&str] = &["ore", "electronics", "fuel"];

/// An operation in the spacesim alphabet.
pub enum SpaceOp {
    /// Move a ship to a different station.
    TravelTo {
        /// Ship name.
        ship: String,
        /// Destination station.
        station: String,
    },
    /// Load cargo from station onto ship.
    LoadCargo {
        /// Ship name.
        ship: String,
        /// Commodity to load.
        commodity: String,
        /// Amount to load.
        amount: i64,
    },
    /// Unload cargo from ship to station.
    UnloadCargo {
        /// Ship name.
        ship: String,
        /// Commodity to unload.
        commodity: String,
        /// Amount to unload.
        amount: i64,
    },
    /// Query a ship's state.
    QueryShip {
        /// Ship name.
        ship: String,
    },
    /// Query a station's state.
    QueryStation {
        /// Station name.
        station: String,
    },
    /// Verify all actors match the reference model.
    VerifyAll,
    /// Small delay for simulation progress.
    SmallDelay,
}

/// Generate a random operation based on model state.
///
/// Weights: TravelTo 20%, LoadCargo 20%, UnloadCargo 15%,
/// QueryShip 10%, QueryStation 10%, VerifyAll 10%, SmallDelay 15%.
pub fn random_op(
    random: &SimRandomProvider,
    model: &SpaceModel,
    stations: &[String],
    ships: &[String],
) -> SpaceOp {
    let roll = random.random_range(0..100);
    let station = || stations[random.random_range(0..stations.len())].clone();
    let ship = || ships[random.random_range(0..ships.len())].clone();

    match roll {
        0..20 => {
            // TravelTo: pick ship, pick station different from current
            let s = ship();
            let current = model.ship_location(&s).unwrap_or("");
            let others: Vec<_> = stations
                .iter()
                .filter(|st| st.as_str() != current)
                .collect();
            if others.is_empty() {
                return SpaceOp::SmallDelay;
            }
            let dest = others[random.random_range(0..others.len())].clone();
            SpaceOp::TravelTo {
                ship: s,
                station: dest,
            }
        }
        20..40 => {
            // LoadCargo: pick ship, find what its station has
            let s = ship();
            let loc = model.ship_location(&s).unwrap_or("").to_string();
            if loc.is_empty() {
                return SpaceOp::SmallDelay;
            }
            let commodity = COMMODITIES[random.random_range(0..COMMODITIES.len())].to_string();
            let available = model
                .stations
                .get(&loc)
                .and_then(|st| st.inventory.get(&commodity))
                .copied()
                .unwrap_or(0);
            if available <= 0 {
                // Station doesn't have this commodity, try anyway with small amount
                return SpaceOp::LoadCargo {
                    ship: s,
                    commodity,
                    amount: random.random_range(1..10),
                };
            }
            let amount = random.random_range(1..(available + 1));
            SpaceOp::LoadCargo {
                ship: s,
                commodity,
                amount,
            }
        }
        40..55 => {
            // UnloadCargo: pick ship with cargo
            let s = ship();
            let ship_data = model.ships.get(&s);
            let has_cargo: Vec<_> = ship_data
                .map(|sd| {
                    sd.cargo
                        .iter()
                        .filter(|&(_, v)| *v > 0)
                        .map(|(k, _)| k.clone())
                        .collect()
                })
                .unwrap_or_default();
            if has_cargo.is_empty() {
                // Ship has nothing, try anyway
                let commodity = COMMODITIES[random.random_range(0..COMMODITIES.len())].to_string();
                return SpaceOp::UnloadCargo {
                    ship: s,
                    commodity,
                    amount: random.random_range(1..10),
                };
            }
            let commodity = has_cargo[random.random_range(0..has_cargo.len())].clone();
            let available = ship_data
                .and_then(|sd| sd.cargo.get(&commodity))
                .copied()
                .unwrap_or(0);
            let amount = random.random_range(1..(available + 1));
            SpaceOp::UnloadCargo {
                ship: s,
                commodity,
                amount,
            }
        }
        55..65 => SpaceOp::QueryShip { ship: ship() },
        65..75 => SpaceOp::QueryStation { station: station() },
        75..85 => SpaceOp::VerifyAll,
        _ => SpaceOp::SmallDelay,
    }
}
