//! Reference model for spacesim invariant checking.
//!
//! Tracks expected station credits and inventory, providing a source of truth
//! for conservation law and non-negative balance invariants.

use std::collections::{BTreeMap, BTreeSet};

use super::actors::{ShipResponse, StationResponse};

/// Key used to publish the reference model into `StateHandle`.
pub const SPACE_MODEL_KEY: &str = "space_model";

/// Reference model for space station economy.
///
/// Tracks expected credits for each station and ship alongside running totals
/// for conservation law verification. Loss tracking absorbs gaps from
/// partial failures in two-phase trades.
#[derive(Debug, Clone, Default)]
pub struct SpaceModel {
    /// Current expected state for each station.
    pub stations: BTreeMap<String, StationState>,
    /// Current expected state for each ship.
    pub ships: BTreeMap<String, ShipState>,
    /// Sum of all injected credits (invariant target).
    pub total_credits: i64,
    /// Sum of all cargo per commodity (invariant target).
    pub total_cargo: BTreeMap<String, i64>,
    /// Stations whose state is uncertain due to MaybeDelivered.
    pub uncertain: BTreeSet<String>,
    /// Ships whose state is uncertain due to MaybeDelivered.
    pub uncertain_ships: BTreeSet<String>,
    /// Credits lost to partial trade failures.
    pub lost_credits: i64,
    /// Cargo lost to partial trade failures, per commodity.
    pub lost_cargo: BTreeMap<String, i64>,
}

/// Per-station expected state.
#[derive(Debug, Clone, Default)]
pub struct StationState {
    /// Current expected credits.
    pub credits: i64,
    /// Current expected inventory.
    pub inventory: BTreeMap<String, i64>,
}

/// Per-ship expected state.
#[derive(Debug, Clone, Default)]
pub struct ShipState {
    /// Current expected credits.
    pub credits: i64,
    /// Current expected cargo.
    pub cargo: BTreeMap<String, i64>,
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

    /// Mark a station as uncertain (delivery ambiguity).
    pub fn mark_uncertain(&mut self, name: &str) {
        self.uncertain.insert(name.to_string());
    }

    /// Check if a station has uncertain state.
    pub fn is_uncertain(&self, name: &str) -> bool {
        self.uncertain.contains(name)
    }

    /// Reconcile a station's model state with the actual response from the actor.
    ///
    /// Overwrites the station's credits and inventory, removes uncertainty,
    /// then recalculates totals from scratch.
    pub fn reconcile(&mut self, name: &str, actual: &StationResponse) {
        let station = self.stations.entry(name.to_string()).or_default();
        station.credits = actual.credits;
        station.inventory = actual.inventory.clone();
        self.uncertain.remove(name);
        self.recalculate_totals();
    }

    // ========================================================================
    // Ship methods
    // ========================================================================

    /// Seed a ship with initial credits.
    pub fn seed_ship(&mut self, name: &str, credits: i64) {
        let ship = self.ships.entry(name.to_string()).or_default();
        ship.credits += credits;
        self.total_credits += credits;
    }

    /// Get credits for a ship.
    pub fn ship_credits(&self, name: &str) -> i64 {
        self.ships.get(name).map(|s| s.credits).unwrap_or(0)
    }

    /// Sum of all ship credits.
    pub fn total_ship_credits(&self) -> i64 {
        self.ships.values().map(|s| s.credits).sum()
    }

    /// Get a ship's cargo for a specific commodity.
    pub fn ship_cargo(&self, name: &str, commodity: &str) -> i64 {
        self.ships
            .get(name)
            .and_then(|s| s.cargo.get(commodity))
            .copied()
            .unwrap_or(0)
    }

    /// Sum of all cargo for a commodity across all ships.
    pub fn total_ship_cargo_for(&self, commodity: &str) -> i64 {
        self.ships
            .values()
            .map(|s| s.cargo.get(commodity).copied().unwrap_or(0))
            .sum()
    }

    /// Mark a ship as uncertain (delivery ambiguity).
    pub fn mark_ship_uncertain(&mut self, name: &str) {
        self.uncertain_ships.insert(name.to_string());
    }

    /// Check if a ship has uncertain state.
    pub fn is_ship_uncertain(&self, name: &str) -> bool {
        self.uncertain_ships.contains(name)
    }

    /// Reconcile a ship's model state with the actual response from the actor.
    pub fn reconcile_ship(&mut self, name: &str, actual: &ShipResponse) {
        let ship = self.ships.entry(name.to_string()).or_default();
        ship.credits = actual.credits;
        ship.cargo = actual.cargo.clone();
        self.uncertain_ships.remove(name);
        self.recalculate_totals();
    }

    /// Apply a buy trade: ship pays credits to station, gets cargo from station.
    ///
    /// Net effect on totals: zero (credits and cargo just move between entities).
    pub fn trade_buy(
        &mut self,
        ship: &str,
        station: &str,
        commodity: &str,
        amount: i64,
        price: i64,
    ) {
        let s = self.ships.entry(ship.to_string()).or_default();
        s.credits -= price;
        *s.cargo.entry(commodity.to_string()).or_insert(0) += amount;

        let st = self.stations.entry(station.to_string()).or_default();
        *st.inventory.entry(commodity.to_string()).or_insert(0) -= amount;
        st.credits += price;
        // Totals unchanged: credits and cargo just moved between entities
    }

    /// Apply a sell trade: ship gives cargo to station, gets credits from station.
    ///
    /// Net effect on totals: zero (credits and cargo just move between entities).
    pub fn trade_sell(
        &mut self,
        ship: &str,
        station: &str,
        commodity: &str,
        amount: i64,
        price: i64,
    ) {
        let s = self.ships.entry(ship.to_string()).or_default();
        *s.cargo.entry(commodity.to_string()).or_insert(0) -= amount;
        s.credits += price;

        let st = self.stations.entry(station.to_string()).or_default();
        *st.inventory.entry(commodity.to_string()).or_insert(0) += amount;
        st.credits -= price;
        // Totals unchanged: credits and cargo just moved between entities
    }

    /// Recalculate totals from all station and ship state.
    ///
    /// Any gap between actual and expected totals is absorbed into
    /// `lost_credits` / `lost_cargo` (from partial trade failures).
    fn recalculate_totals(&mut self) {
        let actual_credits: i64 = self.stations.values().map(|s| s.credits).sum::<i64>()
            + self.ships.values().map(|s| s.credits).sum::<i64>();
        self.lost_credits = self.total_credits - actual_credits;

        // Compute actual cargo across stations and ships
        let mut actual_cargo: BTreeMap<String, i64> = BTreeMap::new();
        for station in self.stations.values() {
            for (c, &a) in &station.inventory {
                *actual_cargo.entry(c.clone()).or_insert(0) += a;
            }
        }
        for ship in self.ships.values() {
            for (c, &a) in &ship.cargo {
                *actual_cargo.entry(c.clone()).or_insert(0) += a;
            }
        }
        self.lost_cargo.clear();
        for (commodity, &expected) in &self.total_cargo {
            let actual = actual_cargo.get(commodity).copied().unwrap_or(0);
            if actual != expected {
                self.lost_cargo.insert(commodity.clone(), expected - actual);
            }
        }
    }
}
