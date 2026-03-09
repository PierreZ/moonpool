//! Reference model for spacesim invariant checking.
//!
//! No uncertainty tracking. The model is always consistent because
//! all mutations use op_id dedup and at-least-once delivery.

use std::collections::BTreeMap;

/// Key used to publish the reference model into `StateHandle`.
pub const SPACE_MODEL_KEY: &str = "space_model";

/// Reference model for the cargo hauling network.
#[derive(Debug, Clone, Default)]
pub struct SpaceModel {
    /// Per-station inventory.
    pub stations: BTreeMap<String, ModelStation>,
    /// Per-ship state.
    pub ships: BTreeMap<String, ModelShip>,
    /// Per-commodity conservation target (sum of all cargo everywhere).
    pub total_cargo: BTreeMap<String, i64>,
}

/// Per-station model state.
#[derive(Debug, Clone, Default)]
pub struct ModelStation {
    /// Current inventory.
    pub inventory: BTreeMap<String, i64>,
}

/// Per-ship model state.
#[derive(Debug, Clone, Default)]
pub struct ModelShip {
    /// Station the ship is docked at.
    pub docked_at: String,
    /// Current cargo manifest.
    pub cargo: BTreeMap<String, i64>,
}

impl SpaceModel {
    /// Create an empty model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a station with initial cargo inventory.
    pub fn seed_station(&mut self, name: &str, commodity: &str, amount: i64) {
        let station = self.stations.entry(name.to_string()).or_default();
        *station.inventory.entry(commodity.to_string()).or_insert(0) += amount;
        *self.total_cargo.entry(commodity.to_string()).or_insert(0) += amount;
    }

    /// Seed a ship at a starting station.
    pub fn seed_ship(&mut self, name: &str, station: &str) {
        let ship = self.ships.entry(name.to_string()).or_default();
        ship.docked_at = station.to_string();
    }

    /// Move a ship to a new station.
    pub fn travel_to(&mut self, ship: &str, station: &str) {
        let s = self.ships.entry(ship.to_string()).or_default();
        s.docked_at = station.to_string();
    }

    /// Transfer cargo from station to ship. Totals unchanged.
    pub fn load_cargo(&mut self, ship: &str, station: &str, commodity: &str, amount: i64) {
        let st = self.stations.entry(station.to_string()).or_default();
        *st.inventory.entry(commodity.to_string()).or_insert(0) -= amount;

        let s = self.ships.entry(ship.to_string()).or_default();
        *s.cargo.entry(commodity.to_string()).or_insert(0) += amount;
    }

    /// Transfer cargo from ship to station. Totals unchanged.
    pub fn unload_cargo(&mut self, ship: &str, station: &str, commodity: &str, amount: i64) {
        let s = self.ships.entry(ship.to_string()).or_default();
        *s.cargo.entry(commodity.to_string()).or_insert(0) -= amount;

        let st = self.stations.entry(station.to_string()).or_default();
        *st.inventory.entry(commodity.to_string()).or_insert(0) += amount;
    }

    /// Check if station has at least `amount` of `commodity`.
    pub fn station_has(&self, station: &str, commodity: &str, amount: i64) -> bool {
        self.stations
            .get(station)
            .and_then(|s| s.inventory.get(commodity))
            .copied()
            .unwrap_or(0)
            >= amount
    }

    /// Check if ship has at least `amount` of `commodity`.
    pub fn ship_has(&self, ship: &str, commodity: &str, amount: i64) -> bool {
        self.ships
            .get(ship)
            .and_then(|s| s.cargo.get(commodity))
            .copied()
            .unwrap_or(0)
            >= amount
    }

    /// Get the station a ship is docked at.
    pub fn ship_location(&self, ship: &str) -> Option<&str> {
        self.ships.get(ship).map(|s| s.docked_at.as_str())
    }
}
