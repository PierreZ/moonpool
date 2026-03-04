//! Invariants for spacesim: credit conservation and non-negative balances.
//!
//! These run after every simulation event, validating cross-workload
//! properties via the published `SpaceModel`.

use moonpool_sim::{Invariant, StateHandle, assert_always};

use super::model::{SPACE_MODEL_KEY, SpaceModel};

/// Credit conservation invariant: sum(station credits) == total_credits.
pub struct CreditConservation;

impl Invariant for CreditConservation {
    fn name(&self) -> &str {
        "credit_conservation"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) {
            let sum = model.total_station_credits();
            assert_always!(
                sum == model.total_credits,
                format!(
                    "credit conservation violated: sum({}) != total({})",
                    sum, model.total_credits
                )
            );
        }
    }
}

/// Non-negative balances invariant: all station credits >= 0.
pub struct NonNegativeBalances;

impl Invariant for NonNegativeBalances {
    fn name(&self) -> &str {
        "non_negative_balances"
    }

    fn check(&self, state: &StateHandle, _sim_time_ms: u64) {
        if let Some(model) = state.get::<SpaceModel>(SPACE_MODEL_KEY) {
            for (name, station) in &model.stations {
                assert_always!(
                    station.credits >= 0,
                    format!(
                        "station '{}' has negative credits: {}",
                        name, station.credits
                    )
                );
            }
        }
    }
}
