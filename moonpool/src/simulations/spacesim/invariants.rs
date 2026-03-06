//! Invariants for spacesim: credit conservation and non-negative balances.
//!
//! These run after every simulation event, validating cross-workload
//! properties via the published `SpaceModel`.

use moonpool_sim::{Invariant, StateHandle, assert_always};

use super::model::{SPACE_MODEL_KEY, SpaceModel};

/// Credit conservation invariant: sum(station + ship credits) + lost == total_credits.
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
            let sum =
                model.total_station_credits() + model.total_ship_credits() + model.lost_credits;
            assert_always!(sum == model.total_credits, "credit conservation violated");
        }
    }
}

/// Cargo conservation invariant: sum(station + ship cargo) + lost == total_cargo for all commodities.
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
                let lost = model.lost_cargo.get(commodity).copied().unwrap_or(0);
                assert_always!(
                    actual_stations + actual_ships + lost == expected,
                    "cargo conservation violated"
                );
            }
        }
    }
}

/// Non-negative balances invariant: all station and ship credits >= 0.
pub struct NonNegativeBalances;

impl Invariant for NonNegativeBalances {
    fn name(&self) -> &str {
        "non_negative_balances"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) {
            if !model.uncertain.is_empty() || !model.uncertain_ships.is_empty() {
                return;
            }
            for station in model.stations.values() {
                assert_always!(station.credits >= 0, "non-negative station credits");
            }
            for ship in model.ships.values() {
                assert_always!(ship.credits >= 0, "non-negative ship credits");
            }
        }
    }
}
