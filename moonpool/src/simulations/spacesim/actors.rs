//! Station and Ship actors for the cargo hauling network.
//!
//! Both actors use op_id dedup for crash-safe idempotency.
//! Ships coordinate with stations for load/unload operations.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::actors::{
    ActorContext, ActorError, ActorHandler, ActorHandlerError, DeactivationHint, PersistentState,
};
use crate::{MessageCodec, Providers, RpcError, actor_impl, service};

// ============================================================================
// Request/Response types
// ============================================================================

/// Request to add or remove cargo from a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationCargoRequest {
    /// Unique operation ID for dedup.
    pub op_id: u64,
    /// Commodity name.
    pub commodity: String,
    /// Amount to add or remove.
    pub amount: i64,
}

/// Request to query a station's current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryStationRequest {}

/// Response from a station actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationResponse {
    /// Current inventory.
    pub inventory: BTreeMap<String, i64>,
    /// Whether the operation succeeded.
    pub success: bool,
}

/// Request for a ship to travel to a station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TravelRequest {
    /// Destination station name.
    pub destination: String,
}

/// Request to load or unload cargo on a ship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoRequest {
    /// Unique operation ID for dedup.
    pub op_id: u64,
    /// Commodity name.
    pub commodity: String,
    /// Amount to load or unload.
    pub amount: i64,
}

/// Request to query a ship's current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryShipRequest {}

/// Response from a ship actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipResponse {
    /// Station the ship is docked at.
    pub docked_at: String,
    /// Current cargo manifest.
    pub cargo: BTreeMap<String, i64>,
}

/// Response from a cargo load/unload operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoResponse {
    /// Whether the operation succeeded.
    pub success: bool,
    /// Ship's cargo after the operation.
    pub ship_cargo: BTreeMap<String, i64>,
    /// Station's inventory after the operation.
    pub station_inventory: BTreeMap<String, i64>,
}

// ============================================================================
// StationActor
// ============================================================================

/// Persistent state for a station actor.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct StationData {
    /// Inventory of goods.
    pub inventory: BTreeMap<String, i64>,
    /// Completed operation IDs for dedup.
    pub completed_ops: BTreeSet<u64>,
}

/// Define the station virtual actor interface.
#[service(id = 0x5741_8000)]
pub trait Station {
    /// Add cargo to the station (idempotent via op_id).
    async fn add_cargo(&mut self, req: StationCargoRequest) -> Result<StationResponse, RpcError>;

    /// Remove cargo from the station (idempotent via op_id).
    async fn remove_cargo(&mut self, req: StationCargoRequest)
    -> Result<StationResponse, RpcError>;

    /// Query the current station state.
    async fn query_state(&mut self, req: QueryStationRequest) -> Result<StationResponse, RpcError>;
}

/// Station actor implementation with persistent state and op_id dedup.
#[derive(Default)]
pub struct StationActorImpl {
    state: Option<PersistentState<StationData>>,
}

impl StationActorImpl {
    fn data(&self) -> StationData {
        self.state
            .as_ref()
            .map(|s| s.state().clone())
            .unwrap_or_default()
    }

    fn snapshot_response(&self, success: bool) -> StationResponse {
        let data = self.data();
        StationResponse {
            inventory: data.inventory,
            success,
        }
    }
}

#[async_trait(?Send)]
impl Station for StationActorImpl {
    async fn add_cargo<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: StationCargoRequest,
    ) -> Result<StationResponse, RpcError> {
        if let Some(s) = &mut self.state {
            if s.state().completed_ops.contains(&req.op_id) {
                return Ok(self.snapshot_response(true));
            }
            *s.state_mut().inventory.entry(req.commodity).or_insert(0) += req.amount;
            s.state_mut().completed_ops.insert(req.op_id);
            let _ = s.write_state().await;
        }
        Ok(self.snapshot_response(true))
    }

    async fn remove_cargo<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: StationCargoRequest,
    ) -> Result<StationResponse, RpcError> {
        if let Some(s) = &mut self.state {
            if s.state().completed_ops.contains(&req.op_id) {
                return Ok(self.snapshot_response(true));
            }
            let current = s
                .state()
                .inventory
                .get(&req.commodity)
                .copied()
                .unwrap_or(0);
            if current < req.amount {
                return Ok(self.snapshot_response(false));
            }
            *s.state_mut().inventory.entry(req.commodity).or_insert(0) -= req.amount;
            s.state_mut().completed_ops.insert(req.op_id);
            let _ = s.write_state().await;
        }
        Ok(self.snapshot_response(true))
    }

    async fn query_state<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        _req: QueryStationRequest,
    ) -> Result<StationResponse, RpcError> {
        Ok(self.snapshot_response(true))
    }
}

#[actor_impl(Station)]
impl ActorHandler for StationActorImpl {
    fn placement_strategy() -> crate::actors::PlacementStrategy {
        crate::actors::PlacementStrategy::RoundRobin
    }

    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateOnIdle
    }

    async fn on_activate<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        if let Some(store) = ctx.state_store() {
            let ps =
                PersistentState::<StationData>::load(store.clone(), "Station", &ctx.id.identity)
                    .await
                    .map_err(|e| {
                        ActorError::from(ActorHandlerError::HandlerError {
                            message: format!("state load: {e}"),
                        })
                    })?;
            self.state = Some(ps);
        }
        Ok(())
    }
}

// ============================================================================
// ShipActor
// ============================================================================

/// Persistent state for a ship actor.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct ShipData {
    /// Station the ship is docked at.
    pub docked_at: String,
    /// Cargo manifest.
    pub cargo: BTreeMap<String, i64>,
    /// Completed operation IDs for dedup.
    pub completed_ops: BTreeSet<u64>,
}

/// Define the ship virtual actor interface.
#[service(id = 0x5348_2000)]
pub trait Ship {
    /// Travel to a station (idempotent state assertion).
    async fn travel_to(&mut self, req: TravelRequest) -> Result<ShipResponse, RpcError>;

    /// Load cargo from the station the ship is docked at.
    async fn load_cargo(&mut self, req: CargoRequest) -> Result<CargoResponse, RpcError>;

    /// Unload cargo to the station the ship is docked at.
    async fn unload_cargo(&mut self, req: CargoRequest) -> Result<CargoResponse, RpcError>;

    /// Query the current ship state.
    async fn query_ship(&mut self, req: QueryShipRequest) -> Result<ShipResponse, RpcError>;
}

/// Ship actor implementation with persistent state and cross-actor coordination.
#[derive(Default)]
pub struct ShipActorImpl {
    state: Option<PersistentState<ShipData>>,
}

impl ShipActorImpl {
    fn data(&self) -> ShipData {
        self.state
            .as_ref()
            .map(|s| s.state().clone())
            .unwrap_or_default()
    }

    fn ship_response(&self) -> ShipResponse {
        let data = self.data();
        ShipResponse {
            docked_at: data.docked_at,
            cargo: data.cargo,
        }
    }
}

#[async_trait(?Send)]
impl Ship for ShipActorImpl {
    async fn travel_to<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        req: TravelRequest,
    ) -> Result<ShipResponse, RpcError> {
        // Strategy 1: pure idempotent state assertion — no op_id needed
        if let Some(s) = &mut self.state {
            s.state_mut().docked_at = req.destination;
            let _ = s.write_state().await;
        }
        Ok(self.ship_response())
    }

    async fn load_cargo<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
        req: CargoRequest,
    ) -> Result<CargoResponse, RpcError> {
        // Dedup check
        if self.data().completed_ops.contains(&req.op_id) {
            return Ok(CargoResponse {
                success: true,
                ship_cargo: self.data().cargo,
                station_inventory: BTreeMap::new(),
            });
        }

        // Call station to release cargo
        let docked_at = self.data().docked_at.clone();
        let station_ref: StationRef<P, C> = ctx.actor_ref(&docked_at);
        let station_resp = station_ref
            .remove_cargo(StationCargoRequest {
                op_id: req.op_id,
                commodity: req.commodity.clone(),
                amount: req.amount,
            })
            .await?;

        if !station_resp.success {
            return Ok(CargoResponse {
                success: false,
                ship_cargo: self.data().cargo,
                station_inventory: station_resp.inventory,
            });
        }

        // Station deducted; add to ship
        if let Some(s) = &mut self.state {
            *s.state_mut().cargo.entry(req.commodity).or_insert(0) += req.amount;
            s.state_mut().completed_ops.insert(req.op_id);
            let _ = s.write_state().await;
        }

        Ok(CargoResponse {
            success: true,
            ship_cargo: self.data().cargo,
            station_inventory: station_resp.inventory,
        })
    }

    async fn unload_cargo<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
        req: CargoRequest,
    ) -> Result<CargoResponse, RpcError> {
        // Dedup check
        if self.data().completed_ops.contains(&req.op_id) {
            return Ok(CargoResponse {
                success: true,
                ship_cargo: self.data().cargo,
                station_inventory: BTreeMap::new(),
            });
        }

        // Check ship has enough cargo
        let current = self.data().cargo.get(&req.commodity).copied().unwrap_or(0);
        if current < req.amount {
            return Ok(CargoResponse {
                success: false,
                ship_cargo: self.data().cargo,
                station_inventory: BTreeMap::new(),
            });
        }

        // Deduct from ship first
        if let Some(s) = &mut self.state {
            *s.state_mut()
                .cargo
                .entry(req.commodity.clone())
                .or_insert(0) -= req.amount;
        }

        // Call station to receive cargo
        let docked_at = self.data().docked_at.clone();
        let station_ref: StationRef<P, C> = ctx.actor_ref(&docked_at);
        let station_resp = station_ref
            .add_cargo(StationCargoRequest {
                op_id: req.op_id,
                commodity: req.commodity,
                amount: req.amount,
            })
            .await?;

        // Mark completed and persist
        if let Some(s) = &mut self.state {
            s.state_mut().completed_ops.insert(req.op_id);
            let _ = s.write_state().await;
        }

        Ok(CargoResponse {
            success: true,
            ship_cargo: self.data().cargo,
            station_inventory: station_resp.inventory,
        })
    }

    async fn query_ship<P: Providers, C: MessageCodec>(
        &mut self,
        _ctx: &ActorContext<P, C>,
        _req: QueryShipRequest,
    ) -> Result<ShipResponse, RpcError> {
        Ok(self.ship_response())
    }
}

#[actor_impl(Ship)]
impl ActorHandler for ShipActorImpl {
    fn placement_strategy() -> crate::actors::PlacementStrategy {
        crate::actors::PlacementStrategy::RoundRobin
    }

    fn deactivation_hint(&self) -> DeactivationHint {
        DeactivationHint::DeactivateOnIdle
    }

    async fn on_activate<P: Providers, C: MessageCodec>(
        &mut self,
        ctx: &ActorContext<P, C>,
    ) -> Result<(), ActorError> {
        if let Some(store) = ctx.state_store() {
            let ps = PersistentState::<ShipData>::load(store.clone(), "Ship", &ctx.id.identity)
                .await
                .map_err(|e| {
                    ActorError::from(ActorHandlerError::HandlerError {
                        message: format!("state load: {e}"),
                    })
                })?;
            self.state = Some(ps);
        }
        Ok(())
    }
}
