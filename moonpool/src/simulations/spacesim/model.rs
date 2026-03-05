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
    /// Sum of all cargo per commodity (invariant target).
    pub total_cargo: BTreeMap<String, i64>,
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

    /// Add cargo to a station.
    pub fn add_cargo(&mut self, name: &str, commodity: &str, amount: i64) {
        let station = self.stations.entry(name.to_string()).or_default();
        *station.inventory.entry(commodity.to_string()).or_insert(0) += amount;
        *self.total_cargo.entry(commodity.to_string()).or_insert(0) += amount;
    }

    /// Remove cargo from a station. Returns true if successful (sufficient cargo).
    pub fn remove_cargo(&mut self, name: &str, commodity: &str, amount: i64) -> bool {
        let station = self.stations.entry(name.to_string()).or_default();
        let current = station.inventory.get(commodity).copied().unwrap_or(0);
        if current >= amount {
            *station.inventory.entry(commodity.to_string()).or_insert(0) -= amount;
            *self.total_cargo.entry(commodity.to_string()).or_insert(0) -= amount;
            true
        } else {
            false
        }
    }

    /// Get a station's cargo for a specific commodity.
    pub fn station_cargo(&self, name: &str, commodity: &str) -> i64 {
        self.stations
            .get(name)
            .and_then(|s| s.inventory.get(commodity))
            .copied()
            .unwrap_or(0)
    }

    /// Get a station's inventory.
    pub fn station_inventory(&self, name: &str) -> Option<&BTreeMap<String, i64>> {
        self.stations.get(name).map(|s| &s.inventory)
    }

    /// Sum of all cargo for a commodity across all stations.
    pub fn total_cargo_for(&self, commodity: &str) -> i64 {
        self.stations
            .values()
            .map(|s| s.inventory.get(commodity).copied().unwrap_or(0))
            .sum()
    }
}
