//! Reference model for spacesim invariant checking.
//!
//! Tracks expected station credits and inventory, providing a source of truth
//! for conservation law and non-negative balance invariants.

use std::collections::BTreeMap;

/// Key used to publish the reference model into `StateHandle`.
pub const SPACE_MODEL_KEY: &str = "space_model";

/// Reference model for space station economy.
///
/// Tracks expected credits for each station alongside a running total
/// for conservation law verification.
#[derive(Debug, Clone, Default)]
pub struct SpaceModel {
    /// Current expected state for each station.
    pub stations: BTreeMap<String, StationState>,
    /// Sum of all station credits (invariant target).
    pub total_credits: i64,
}

/// Per-station expected state.
#[derive(Debug, Clone, Default)]
pub struct StationState {
    /// Current expected credits.
    pub credits: i64,
    /// Current expected inventory.
    pub inventory: BTreeMap<String, i64>,
}

impl SpaceModel {
    /// Create an empty model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a station with initial credits.
    pub fn seed_station(&mut self, name: &str, credits: i64) {
        let station = self.stations.entry(name.to_string()).or_default();
        station.credits += credits;
        self.total_credits += credits;
    }

    /// Apply a deposit to a station.
    pub fn deposit(&mut self, name: &str, amount: i64) {
        let station = self.stations.entry(name.to_string()).or_default();
        station.credits += amount;
        self.total_credits += amount;
    }

    /// Apply a withdrawal. Returns true if successful (sufficient funds).
    pub fn withdraw(&mut self, name: &str, amount: i64) -> bool {
        let station = self.stations.entry(name.to_string()).or_default();
        if station.credits >= amount {
            station.credits -= amount;
            self.total_credits -= amount;
            true
        } else {
            false
        }
    }

    /// Get credits for a station.
    pub fn station_credits(&self, name: &str) -> i64 {
        self.stations.get(name).map(|s| s.credits).unwrap_or(0)
    }

    /// Sum of all station credits.
    pub fn total_station_credits(&self) -> i64 {
        self.stations.values().map(|s| s.credits).sum()
    }

    /// Get a station's inventory.
    pub fn station_inventory(&self, name: &str) -> Option<&BTreeMap<String, i64>> {
        self.stations.get(name).map(|s| &s.inventory)
    }
}
