//! Invariants for the cargo hauling network.
//!
//! Always-on: no uncertainty guards needed because op_id dedup
//! guarantees the model is always consistent.

use moonpool_sim::{Invariant, StateHandle, assert_always};

use super::model::{SPACE_MODEL_KEY, SpaceModel};

/// Cargo conservation: sum of all cargo across stations and ships
/// must equal the seeded totals for each commodity.
pub struct CargoConservation;

impl Invariant for CargoConservation {
    fn name(&self) -> &str {
        "cargo_conservation"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) else {
            return;
        };
        for (commodity, &expected) in &model.total_cargo {
            let station_sum: i64 = model
                .stations
                .values()
                .map(|s| s.inventory.get(commodity).copied().unwrap_or(0))
                .sum();
            let ship_sum: i64 = model
                .ships
                .values()
                .map(|s| s.cargo.get(commodity).copied().unwrap_or(0))
                .sum();
            let actual = station_sum + ship_sum;
            assert_always!(actual == expected, "cargo conservation violated", {
                "commodity" => commodity, "actual" => actual, "expected" => expected
            });
        }
    }
}

/// All inventory and cargo values must be non-negative.
pub struct NonNegativeInventory;

impl Invariant for NonNegativeInventory {
    fn name(&self) -> &str {
        "non_negative_inventory"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) else {
            return;
        };
        for (name, station) in &model.stations {
            for (commodity, &amount) in &station.inventory {
                assert_always!(amount >= 0, "non-negative station inventory", {
                    "station" => name, "commodity" => commodity, "amount" => amount
                });
            }
        }
        for (name, ship) in &model.ships {
            for (commodity, &amount) in &ship.cargo {
                assert_always!(amount >= 0, "non-negative ship cargo", {
                    "ship" => name, "commodity" => commodity, "amount" => amount
                });
            }
        }
    }
}
