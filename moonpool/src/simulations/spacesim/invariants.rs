//! Invariants for spacesim: credit conservation and non-negative balances.
//!
//! These run after every simulation event, validating cross-workload
//! properties via the published `SpaceModel`.

use moonpool_sim::{Invariant, StateHandle, assert_always};

use super::model::{SPACE_MODEL_KEY, SpaceModel};

/// Credit conservation invariant: sum(station + ship credits) == total_credits.
///
/// Skipped when any entity is uncertain since partial sums can't prove conservation.
pub struct CreditConservation;

impl Invariant for CreditConservation {
    fn name(&self) -> &str {
        "credit_conservation"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) {
            if !model.uncertain.is_empty() || !model.uncertain_ships.is_empty() {
                return;
            }
            let sum = model.total_station_credits() + model.total_ship_credits();
            assert_always!(sum == model.total_credits, "credit conservation violated", {
                "sum" => sum, "expected" => model.total_credits
            });
        }
    }
}

/// Cargo conservation invariant: sum(station + ship cargo) == total_cargo for all commodities.
///
/// Skipped when any entity is uncertain since partial sums can't prove conservation.
pub struct CargoConservation;

impl Invariant for CargoConservation {
    fn name(&self) -> &str {
        "cargo_conservation"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) {
            if !model.uncertain.is_empty() || !model.uncertain_ships.is_empty() {
                return;
            }
            for (commodity, &expected) in &model.total_cargo {
                let actual_stations = model.total_cargo_for(commodity);
                let actual_ships = model.total_ship_cargo_for(commodity);
                assert_always!(
                    actual_stations + actual_ships == expected,
                    "cargo conservation violated",
                    { "commodity" => commodity, "actual" => actual_stations + actual_ships, "expected" => expected }
                );
            }
        }
    }
}

/// Non-negative balances invariant: all certain station and ship credits >= 0.
///
/// Checks per-entity, skipping uncertain ones (instead of bailing entirely).
pub struct NonNegativeBalances;

impl Invariant for NonNegativeBalances {
    fn name(&self) -> &str {
        "non_negative_balances"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) {
            for (name, station) in &model.stations {
                if model.uncertain.contains(name) {
                    continue;
                }
                assert_always!(station.credits >= 0, "non-negative station credits", {
                    "station" => name, "credits" => station.credits
                });
            }
            for (name, ship) in &model.ships {
                if model.uncertain_ships.contains(name) {
                    continue;
                }
                assert_always!(ship.credits >= 0, "non-negative ship credits", {
                    "ship" => name, "credits" => ship.credits
                });
            }
        }
    }
}
